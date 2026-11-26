//! SQLite's positions of a connected plan (#2950).
//!
//! Implements the 7 required methods of [`PlanSession`] and the 4 provided
//! plan-gate methods that need no new catalog read
//! (`require_transactional_rebuilds`, `vet_plan_sites`, `support_rows`,
//! `server_limits`); the catalog-reading positions
//! (`refuse_occupied_relation_names`, `check_names_against_catalog`,
//! `schedule`, `order_by_catalog`, `check_final_order`) keep their refusing
//! defaults until their SQLite catalog passes are built.

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

/// Shared connection, library, ledger and `strategy: online` preflight for
/// both [`PlanSession::server_limits`] (`Stage::Plan`) and
/// [`ApplySession::preflight_limits`] (`Stage::Apply`), so the two gates
/// cannot drift.
pub(in crate::engine) fn preflight_verdict(
    dialect: &Sqlite,
    conn: &mut Conn,
    changes: &ChangeSet,
    stage: pbps_sqlite::preflight::Stage,
) -> anyhow::Result<EditionVerdict> {
    let c = sqlite_conn(conn)?;
    let online: Vec<String> = changes
        .changes
        .iter()
        .filter(|p| p.strategy.online)
        .map(|p| pbps_model::escape::Escaped(&p.change.subject()).to_string())
        .collect();
    if !online.is_empty() {
        anyhow::bail!(
            "SQLite does not support `strategy: online` (requested for {}); remove \
             `strategy: online` and plan again",
            online.join(", ")
        );
    }
    let verdict = pbps_sqlite::preflight::before_apply(c, changes, dialect.floor(), stage)?;
    if !verdict.problems.is_empty() {
        anyhow::bail!("{}", pbps_sqlite::preflight::refusal(&verdict.problems));
    }
    Ok(EditionVerdict {
        refused_online: Vec::new(),
        runs: format!("SQLite {}", c.version()?),
        warnings: verdict.notes,
    })
}

/// Does not exist on this engine (partitioned-table rebuilds) plus SQLite's
/// staged-view-edit refusal: SQLite replaces a view with `DROP VIEW` followed
/// by `CREATE VIEW` rather than `CREATE OR ALTER`, so a staged run would leave
/// the view absent between two commits, while SQLite runs DDL in a transaction
/// so an unstaged apply is atomic. Shared with [`ApplySession::rebuild_mode`]
/// so plan and apply cannot drift.
pub(in crate::engine) fn require_transactional_rebuilds(
    changes: &ChangeSet,
    staged: bool,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        !changes.changes.iter().any(|p| matches!(
            p.change,
            pbps_model::Change::Partition(pbps_model::PartitionChange::RebuildPartitionedTable(_))
        )),
        "SQLite does not support partitioned-table rebuilds"
    );
    defaults::staged_rebuilds(changes, staged, || {
        anyhow::ensure!(
            !staged || rebuilt_modules(changes).is_empty(),
            "SQLite replaces a view with DROP VIEW and CREATE VIEW, so a staged run would leave \
             the view absent between two commits; SQLite runs DDL in a transaction, so apply \
             without --staged"
        );
        Ok(())
    })
}

/// Does not exist on this engine: SQLite has no system-versioned tables, so
/// a plan that creates one was not built for SQLite. Asked of the table
/// itself and not through SQL Server's `temporal::needs_the_question`, which
/// is also true of every cascading foreign key (a SQL Server 2016 limit) and
/// would refuse an ordinary SQLite table. Shared with
/// [`ApplySession::refuse_unsupported_temporal`] so plan and apply cannot
/// drift.
pub(in crate::engine) fn refuse_system_versioned(changes: &ChangeSet) -> anyhow::Result<()> {
    anyhow::ensure!(
        !changes.changes.iter().any(|p| matches!(
            &p.change,
            pbps_model::Change::Table(pbps_model::TableChange::CreateTable { table, .. })
                if table.system_time.is_some()
        )),
        "SQLite does not support system-versioned tables"
    );
    Ok(())
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

    /// Does not exist on this engine (partitioned-table rebuilds) plus
    /// SQLite's staged-view-edit refusal: delegates to the helper shared with
    /// [`ApplySession::rebuild_mode`].
    fn require_transactional_rebuilds(
        &self,
        changes: &ChangeSet,
        staged: bool,
    ) -> anyhow::Result<()> {
        require_transactional_rebuilds(changes, staged)
    }

    /// Does not exist on this engine (stored routines and `USING` conversions)
    /// plus neutral drop-requirement vetting: SQLite has no stored routines a
    /// table expression can call, so a `DEFAULT`, a `CHECK` or a partial-index
    /// `WHERE` predicate has nothing to vet. A recorded `USING` conversion is
    /// refused by name via [`ApplySession::conversions`]. Any `drop_guard`
    /// runs through [`vet_drop_requirements`] and [`ApplySession::requirement`],
    /// which refuses by name until SQLite's predicate binder is built.
    async fn vet_plan_sites(
        &self,
        state: &PlanState,
        conn: &mut Conn,
        names: &pbps_model::plan_names::PlanNames<'_>,
        _on_note: &mut dyn FnMut(String),
    ) -> anyhow::Result<()> {
        let _ = sqlite_conn(conn)?;
        vet_drop_requirements(self, conn, &state.admission, names).await?;
        self.conversions(conn, &state.admission, names.changes())
            .await?;
        Ok(())
    }

    /// Does not exist on this engine (roles, grants, revokes, row-security
    /// policies, exclusion constraints) or not built yet (generated columns):
    /// delegates each feature guard to [`ApplySession`] so plan and apply
    /// refuse the same foreign plans, and answers all four rows with
    /// `not_applicable` naming SQLite. Unlike `defaults::support_rows`, a
    /// plain column drop or retype is not refused here.
    async fn support_rows(
        &self,
        conn: &mut Conn,
        changes: &ChangeSet,
        ids: &IdsFile,
        reads: SupportReads<'_>,
    ) -> anyhow::Result<SupportRows> {
        let _ = sqlite_conn(conn)?;
        self.permission_support(conn, changes, ids).await?;
        let generation_support = self.generation_support(conn, changes, ids).await?;
        self.owned_targets(changes, ids, reads.owners, reads.session_role)?;
        self.unrevocable_grants(changes, ids, reads.unrevocable)?;
        Ok(SupportRows {
            permission_support: defaults::not_applicable(
                "SQLite",
                "permission_support",
                "SQLite has no permissions, grants or row-security policies",
            ),
            generation_support,
            owned_targets: defaults::not_applicable(
                "SQLite",
                "owned_targets",
                "SQLite has no roles, object owners or grants",
            ),
            unrevocable_grants: defaults::not_applicable(
                "SQLite",
                "unrevocable_grants",
                "SQLite has no grants or revokes",
            ),
        })
    }

    /// Exists in `pbps-sqlite` already plus "does not exist on this engine"
    /// for server editions and system-versioned tables: delegates to the
    /// helper shared with [`ApplySession::preflight_limits`] at `Stage::Plan`
    /// (skipping the file-writability probe), surfaces floor notes through
    /// `edition.warnings`, refuses `strategy: online` on any change, and
    /// refuses any system-versioned table by name. Ordinary cascading foreign
    /// keys and `NOT NULL DEFAULT` additions pass.
    async fn server_limits(
        &self,
        conn: &mut Conn,
        changes: &ChangeSet,
    ) -> anyhow::Result<ServerLimits> {
        let edition = preflight_verdict(self, conn, changes, pbps_sqlite::preflight::Stage::Plan)?;
        refuse_system_versioned(changes)?;
        Ok(ServerLimits {
            edition,
            temporal: Vec::new(),
        })
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

    fn uid(kind: pbps_model::UidKind, name: &str) -> pbps_model::Uid {
        pbps_model::Uid::derived(kind, name, 0)
    }

    fn change_set(changes: Vec<pbps_model::Change>) -> ChangeSet {
        ChangeSet {
            changes: changes
                .into_iter()
                .map(pbps_model::PlannedChange::new)
                .collect(),
        }
    }

    fn view_module(sql: &str) -> Box<pbps_model::Module> {
        Box::new(pbps_model::Module {
            kind: pbps_model::ModuleKind::View,
            description: None,
            definition: sql.into(),
            options: Default::default(),
        })
    }

    #[test]
    fn support_rows_returns_not_applicable_for_plain_plans_including_column_drops_and_retypes() {
        use pbps_model::{
            Change, Column, ColumnChange, ConstraintChange, ExclusionConstraint, Generated,
            GrantChange, GrantTarget, IndexMethod, Permission, PolicyChange, RoleName, Table,
            TableChange, ValueSource,
        };

        let (_tmp, mut conn) = TempDb::create();
        let dialect = Sqlite::default();
        let table = TableName::new("main", "items");
        let t_uid = uid(pbps_model::UidKind::Table, "main.items");
        let drop_uid = uid(pbps_model::UidKind::Column, "main.items.old_col");
        let retype_uid = uid(pbps_model::UidKind::Column, "main.items.qty");
        let ids_of = |changes: &ChangeSet| crate::plan_fixture::ids(changes);

        let owners = BTreeMap::new();
        let session_role = RoleName::new("");
        let reads = SupportReads {
            owners: &owners,
            session_role: &session_role,
            unrevocable: &[],
        };

        // Plain SQLite plan including column drop and column retype (both
        // refused by `defaults::support_rows` via `row_security::column_edit`).
        let plain = change_set(vec![
            Change::Column(ColumnChange::DropColumn {
                uid: drop_uid,
                column: table.column("old_col"),
            }),
            Change::Column(ColumnChange::AlterColumnType {
                uid: retype_uid,
                column: table.column("qty"),
                from: "integer".parse().expect("type"),
                to: "real".parse().expect("type"),
                from_nullable: true,
                to_nullable: true,
                from_collation: None,
                to_collation: None,
                through_external_partitions: false,
                using: None,
            }),
        ]);
        let rows = now(dialect.support_rows(&mut conn, &plain, &ids_of(&plain), reads))
            .expect("plain plan passes");
        for (row, name) in [
            (&rows.permission_support, "permission_support"),
            (&rows.generation_support, "generation_support"),
            (&rows.owned_targets, "owned_targets"),
            (&rows.unrevocable_grants, "unrevocable_grants"),
        ] {
            assert_eq!(row.name, name);
            assert_eq!(row.engine, "SQLite");
            assert_eq!(row.status, "not_applicable");
            assert!(row.message.contains("SQLite"), "{row:?}");
        }

        // Negative: foreign permission, policy, generated-column and exclusion
        // plans are refused naming SQLite and the unsupported feature.
        let grant = change_set(vec![Change::Permission(GrantChange::Grant {
            role: RoleName::new("app"),
            target: GrantTarget::Object(table.clone()),
            permissions: BTreeSet::from([Permission::Select]),
        })]);
        let grant_err =
            now(dialect.support_rows(&mut conn, &grant, &ids_of(&grant), reads)).unwrap_err();
        assert!(
            grant_err.to_string().contains("SQLite") && grant_err.to_string().contains("grants"),
            "{grant_err}"
        );

        let revoke = change_set(vec![Change::Permission(GrantChange::Revoke {
            role: RoleName::new("app"),
            target: GrantTarget::Object(table.clone()),
            permissions: BTreeSet::from([Permission::Select]),
        })]);
        let revoke_err =
            now(dialect.support_rows(&mut conn, &revoke, &ids_of(&revoke), reads)).unwrap_err();
        assert!(
            revoke_err.to_string().contains("SQLite")
                && revoke_err.to_string().contains("permissions"),
            "{revoke_err}"
        );

        let policy = change_set(vec![Change::Policy(PolicyChange::DropPolicy {
            table: table.clone(),
            name: "p".into(),
            policy: Default::default(),
        })]);
        let policy_err =
            now(dialect.support_rows(&mut conn, &policy, &ids_of(&policy), reads)).unwrap_err();
        assert!(
            policy_err.to_string().contains("SQLite")
                && policy_err.to_string().contains("policies"),
            "{policy_err}"
        );

        let unrevocable = [pbps_db::catalog::Unrevocable {
            role: RoleName::new("app"),
            target: GrantTarget::Object(table.clone()),
            permission: Permission::Select,
            why: "granted by another role".into(),
        }];
        let reads_with_unrevocable = SupportReads {
            owners: &owners,
            session_role: &session_role,
            unrevocable: &unrevocable,
        };
        let empty = ChangeSet::default();
        let unrev_err = now(dialect.support_rows(
            &mut conn,
            &empty,
            &IdsFile::default(),
            reads_with_unrevocable,
        ))
        .unwrap_err();
        assert!(
            unrev_err.to_string().contains("SQLite") && unrev_err.to_string().contains("revokes"),
            "{unrev_err}"
        );

        let alter_expr = change_set(vec![Change::Column(ColumnChange::AlterColumnExpression {
            uid: uid(pbps_model::UidKind::Column, "main.items.g"),
            column: table.column("g"),
            from: "a + 1".into(),
            to: "a + 2".into(),
        })]);
        let gen_err =
            now(dialect.support_rows(&mut conn, &alter_expr, &ids_of(&alter_expr), reads))
                .unwrap_err();
        assert!(
            gen_err.to_string().contains("SQLite")
                && gen_err.to_string().contains("generated columns"),
            "{gen_err}"
        );

        let mut gen_col = Column::new("integer".parse().expect("type"));
        gen_col.value_source = ValueSource::Generated(Generated {
            expression: "id + 1".into(),
            stored: false,
        });
        let add_gen = change_set(vec![Change::Column(ColumnChange::AddColumn {
            uid: uid(pbps_model::UidKind::Column, "main.items.g2"),
            table: table.clone(),
            name: "g2".into(),
            column: Box::new(gen_col.clone()),
        })]);
        let add_gen_err =
            now(dialect.support_rows(&mut conn, &add_gen, &ids_of(&add_gen), reads)).unwrap_err();
        assert!(
            add_gen_err.to_string().contains("SQLite")
                && add_gen_err.to_string().contains("generated columns"),
            "{add_gen_err}"
        );

        let mut gen_table = Table::default();
        gen_table.columns.insert("g".into(), gen_col);
        let create_gen = change_set(vec![Change::Table(TableChange::CreateTable {
            uid: t_uid,
            name: table.clone(),
            table: Box::new(gen_table),
            beside_default: None,
            moves_at_most: None,
        })]);
        let create_gen_err =
            now(dialect.support_rows(&mut conn, &create_gen, &ids_of(&create_gen), reads))
                .unwrap_err();
        assert!(
            create_gen_err.to_string().contains("SQLite")
                && create_gen_err.to_string().contains("generated columns"),
            "{create_gen_err}"
        );

        let exclusion = change_set(vec![Change::Constraint(ConstraintChange::SetExclusion {
            table,
            name: "ex".into(),
            from: None,
            to: Some(Box::new(ExclusionConstraint {
                method: IndexMethod::Gist,
                keys: Vec::new(),
                include: Vec::new(),
                filter: None,
                storage_parameters: BTreeMap::new(),
                timing: Default::default(),
            })),
        })]);
        let ex_err = now(dialect.support_rows(&mut conn, &exclusion, &ids_of(&exclusion), reads))
            .unwrap_err();
        assert!(
            ex_err.to_string().contains("SQLite") && ex_err.to_string().contains("exclusion"),
            "{ex_err}"
        );
    }

    #[test]
    fn server_limits_runs_stage_plan_preflight_and_refuses_online_and_temporal_plans() {
        use pbps_model::{
            Change, Column, ColumnChange, ForeignKey, PlannedChange, ReferentialAction, Strategy,
            SystemTime, Table, TableChange, ValueSource,
        };

        let (_tmp, mut conn) = TempDb::create();
        let dialect = Sqlite::default();

        // Plain plan carrying both a `NOT NULL DEFAULT` column addition and an
        // ordinary table with `ON DELETE CASCADE` (both refused by
        // `defaults::server_limits`).
        let mut required_default = Column::new("integer".parse().expect("type"));
        required_default.nullable = false;
        required_default.value_source = ValueSource::Default("0".into());

        let mut child = Table::default();
        child.columns.insert(
            "parent_id".into(),
            Column::new("integer".parse().expect("type")),
        );
        child.foreign_keys.insert(
            "fk_parent".into(),
            ForeignKey {
                columns: vec!["parent_id".into()],
                references_table: TableName::new("main", "parent"),
                references_columns: vec!["id".into()],
                on_delete: ReferentialAction::Cascade,
                on_update: Default::default(),
            },
        );
        let plain = change_set(vec![
            Change::Column(ColumnChange::AddColumn {
                uid: uid(pbps_model::UidKind::Column, "main.t.n"),
                table: TableName::new("main", "t"),
                name: "n".into(),
                column: Box::new(required_default),
            }),
            Change::Table(TableChange::CreateTable {
                uid: uid(pbps_model::UidKind::Table, "main.child"),
                name: TableName::new("main", "child"),
                table: Box::new(child),
                beside_default: None,
                moves_at_most: None,
            }),
        ]);

        let limits = now(dialect.server_limits(&mut conn, &plain)).expect("plain plan passes");
        // The apply asks the same question and must not refuse what the plan
        // accepted: a cascading key is not a system-versioned table.
        now(dialect.refuse_unsupported_temporal(&mut conn, &plain))
            .expect("the apply accepts a cascading key too");
        assert!(limits.edition.refused_online.is_empty());
        assert!(
            limits.edition.runs.starts_with("SQLite 3."),
            "{}",
            limits.edition.runs
        );
        assert!(limits.temporal.is_empty());
        // Default floor 3.35.0 is older than the bundled 3.54.x library, so
        // the floor note surfaces through `edition.warnings`.
        assert_eq!(
            limits.edition.warnings.len(),
            1,
            "{:?}",
            limits.edition.warnings
        );
        assert!(
            limits.edition.warnings[0].contains("floor"),
            "{}",
            limits.edition.warnings[0]
        );

        // With a floor at or above the bundled library, no floor note is emitted.
        let high_floor = Sqlite::with_floor(pbps_sqlite::Version::new(3, 99, 0));
        let quiet = now(high_floor.server_limits(&mut conn, &plain)).expect("high floor passes");
        assert!(
            quiet.edition.warnings.is_empty(),
            "{:?}",
            quiet.edition.warnings
        );

        // Negative: a bad connection pragma is refused via `preflight::refusal`.
        now(conn.execute("PRAGMA legacy_alter_table = ON")).expect("set pragma");
        let pragma_err = now(dialect.server_limits(&mut conn, &plain)).unwrap_err();
        assert!(
            pragma_err.to_string().contains("legacy_alter_table")
                && pragma_err.to_string().contains("Nothing was changed"),
            "{pragma_err}"
        );
        now(conn.execute("PRAGMA legacy_alter_table = OFF")).expect("restore pragma");

        // Negative: `strategy: online` is refused naming SQLite (even on a
        // change SQL Server's `takes_online` ignores).
        let mut online = PlannedChange::new(Change::Table(TableChange::DropTable {
            uid: uid(pbps_model::UidKind::Table, "main.t"),
            name: TableName::new("main", "t"),
            detach_from: None,
        }));
        online.strategy = Strategy { online: true };
        let online_err = now(dialect.server_limits(
            &mut conn,
            &ChangeSet {
                changes: vec![online],
            },
        ))
        .unwrap_err();
        assert!(
            online_err.to_string().contains("SQLite") && online_err.to_string().contains("online"),
            "{online_err}"
        );

        // Negative: a system-versioned table is refused naming SQLite.
        let temporal_table = Table {
            system_time: Some(SystemTime {
                start: "valid_from".into(),
                end: "valid_to".into(),
                hidden: false,
                versioning: None,
            }),
            ..Default::default()
        };
        let temporal = change_set(vec![Change::Table(TableChange::CreateTable {
            uid: uid(pbps_model::UidKind::Table, "main.hist"),
            name: TableName::new("main", "hist"),
            table: Box::new(temporal_table),
            beside_default: None,
            moves_at_most: None,
        })]);
        let temporal_err = now(dialect.server_limits(&mut conn, &temporal)).unwrap_err();
        assert!(
            temporal_err.to_string().contains("SQLite")
                && temporal_err.to_string().contains("system-versioned"),
            "{temporal_err}"
        );
        let at_apply = now(dialect.refuse_unsupported_temporal(&mut conn, &temporal)).unwrap_err();
        assert_eq!(at_apply.to_string(), temporal_err.to_string());
    }

    #[test]
    fn require_transactional_rebuilds_refuses_staged_view_edits_and_partition_rebuilds() {
        use pbps_model::{
            Change, ModuleChange, ModuleId, ModuleKind, PartitionChange, Rebuild,
            RebuildIdentities, RebuildPartitionedTable, TableChange,
        };

        let dialect = Sqlite::default();
        let view_id = ModuleId::Named(TableName::new("main", "v"));

        let alter_view = change_set(vec![Change::Module(ModuleChange::AlterModule {
            id: view_id.clone(),
            module: view_module("SELECT 1"),
        })]);
        dialect
            .require_transactional_rebuilds(&alter_view, false)
            .expect("unstaged view edit passes");

        // Plain non-rebuild change passes even when staged.
        let create_table = change_set(vec![Change::Table(TableChange::CreateTable {
            uid: uid(pbps_model::UidKind::Table, "main.t"),
            name: TableName::new("main", "t"),
            table: Default::default(),
            beside_default: None,
            moves_at_most: None,
        })]);
        dialect
            .require_transactional_rebuilds(&create_table, true)
            .expect("staged create table passes");

        // Negative: staged view edit is refused naming SQLite and telling the
        // operator to apply without `--staged`, aligned with `ApplySession::rebuild_mode`.
        let staged_err = dialect
            .require_transactional_rebuilds(&alter_view, true)
            .unwrap_err();
        let staged_msg = staged_err.to_string();
        assert!(
            staged_msg.contains("SQLite")
                && staged_msg.contains("absent")
                && staged_msg.contains("without --staged"),
            "{staged_msg}"
        );
        let apply_msg = dialect
            .rebuild_mode(&alter_view, true)
            .unwrap_err()
            .to_string();
        assert_eq!(staged_msg, apply_msg);

        // Negative: drop+create view rebuild is also refused when staged.
        let drop_create = change_set(vec![
            Change::Module(ModuleChange::DropModule {
                id: view_id.clone(),
                kind: ModuleKind::View,
            }),
            Change::Module(ModuleChange::CreateModule {
                id: view_id,
                module: view_module("SELECT 2"),
            }),
        ]);
        let dc_err = dialect
            .require_transactional_rebuilds(&drop_create, true)
            .unwrap_err();
        assert!(
            dc_err.to_string().contains("SQLite")
                && dc_err.to_string().contains("without --staged"),
            "{dc_err}"
        );
        dialect
            .require_transactional_rebuilds(&drop_create, false)
            .expect("unstaged drop+create passes");

        // Negative: partitioned-table rebuild is refused both staged and
        // unstaged, naming SQLite.
        let part_rebuild = change_set(vec![Change::Partition(
            PartitionChange::RebuildPartitionedTable(Box::new(RebuildPartitionedTable {
                table: TableName::new("main", "p"),
                intent: Rebuild {
                    source: RebuildIdentities {
                        tables: BTreeMap::new(),
                        columns: BTreeMap::new(),
                    },
                    replacement: RebuildIdentities {
                        tables: BTreeMap::new(),
                        columns: BTreeMap::new(),
                    },
                    replacement_root: uid(pbps_model::UidKind::Table, "main.p_new"),
                    continues: BTreeMap::new(),
                    target_checksum: "0".repeat(64),
                    max_rows: 0,
                    allowed_at: "2026-01-01T00:00:00Z".into(),
                    reason: pbps_model::Reason::new("test").expect("reason"),
                    operator: "test".into(),
                    types: BTreeMap::new(),
                    conversions: BTreeMap::new(),
                    exclusions: BTreeMap::new(),
                },
                earlier: Vec::new(),
                before: BTreeMap::new(),
                after: BTreeMap::new(),
            })),
        )]);
        for staged in [false, true] {
            let err = dialect
                .require_transactional_rebuilds(&part_rebuild, staged)
                .unwrap_err();
            assert!(
                err.to_string().contains("SQLite") && err.to_string().contains("partitioned-table"),
                "staged={staged}: {err}"
            );
            assert_eq!(
                err.to_string(),
                dialect
                    .rebuild_mode(&part_rebuild, staged)
                    .unwrap_err()
                    .to_string()
            );
        }
    }

    #[test]
    fn vet_plan_sites_accepts_table_expressions_and_refuses_using_and_guarded_drops() {
        use pbps_model::{
            Change, CheckConstraint, Column, ColumnChange, ConstraintChange, DropGuard,
            DropRequirement, Index, IndexChange, IndexColumn, PlannedChange, Table, TableChange,
            ValueSource,
        };

        let (_tmp, mut conn) = TempDb::create();
        let dialect = Sqlite::default();
        let state = PlanState::default();
        let table_name = TableName::new("main", "items");
        let t_uid = uid(pbps_model::UidKind::Table, "main.items");

        let mut table = Table::default();
        let mut active = Column::new("integer".parse().expect("type"));
        active.value_source = ValueSource::Default("1".into());
        table.columns.insert("active".into(), active.clone());
        table.checks.insert(
            "ck_active".into(),
            CheckConstraint {
                expression: "active IN (0, 1)".into(),
            },
        );

        let expr_changes = vec![
            PlannedChange::new(Change::Table(TableChange::CreateTable {
                uid: t_uid.clone(),
                name: table_name.clone(),
                table: Box::new(table),
                beside_default: None,
                moves_at_most: None,
            })),
            PlannedChange::new(Change::Column(ColumnChange::AddColumn {
                uid: uid(pbps_model::UidKind::Column, "main.items.extra"),
                table: table_name.clone(),
                name: "extra".into(),
                column: Box::new(active),
            })),
            PlannedChange::new(Change::Constraint(ConstraintChange::AddCheck {
                table: table_name.clone(),
                name: "ck_extra".into(),
                constraint: CheckConstraint {
                    expression: "extra >= 0".into(),
                },
            })),
            PlannedChange::new(Change::Index(IndexChange::AddIndex {
                table: table_name.clone(),
                name: "ix_active".into(),
                index: Box::new(Index {
                    columns: vec![IndexColumn::column("active")],
                    include: Vec::new(),
                    unique: false,
                    filter: Some("active = 1".into()),
                    method: Default::default(),
                    storage_parameters: Default::default(),
                    filegroup: None,
                }),
                clustered: false,
            })),
        ];
        let expr_cs = ChangeSet {
            changes: expr_changes,
        };
        let expr_ids = crate::plan_fixture::ids(&expr_cs);
        let expr_names =
            pbps_model::plan_names::PlanNames::of(&expr_cs.changes, &expr_ids).expect("names");
        let mut notes = Vec::new();
        now(
            dialect.vet_plan_sites(&state, &mut conn, &expr_names, &mut |note| {
                notes.push(note);
            }),
        )
        .expect("default, check and partial index pass");
        assert!(notes.is_empty(), "{notes:?}");

        // Negative: a recorded `USING` conversion is refused naming SQLite.
        let using_cs = ChangeSet {
            changes: vec![PlannedChange::new(Change::Column(
                ColumnChange::AlterColumnType {
                    uid: uid(pbps_model::UidKind::Column, "main.items.active"),
                    column: table_name.column("active"),
                    from: "integer".parse().expect("type"),
                    to: "text".parse().expect("type"),
                    from_nullable: true,
                    to_nullable: true,
                    from_collation: None,
                    to_collation: None,
                    through_external_partitions: false,
                    using: Some("CAST(active AS text)".into()),
                },
            ))],
        };
        let using_ids = crate::plan_fixture::ids(&using_cs);
        let using_names =
            pbps_model::plan_names::PlanNames::of(&using_cs.changes, &using_ids).expect("names");
        let using_err =
            now(dialect.vet_plan_sites(&state, &mut conn, &using_names, &mut |_| {})).unwrap_err();
        assert!(
            using_err.to_string().contains("SQLite") && using_err.to_string().contains("USING"),
            "{using_err}"
        );

        // Negative: a guarded drop does not pass unvetted; it reaches
        // `ApplySession::requirement`, which refuses naming SQLite.
        let mut requirements = BTreeMap::new();
        requirements.insert(
            t_uid.clone(),
            DropRequirement {
                table_uid: t_uid.clone(),
                predicate: "active = 0".into(),
                required_at: "2026-01-01T00:00:00Z".into(),
                operator: "test".into(),
            },
        );
        let mut guarded = PlannedChange::new(Change::Table(TableChange::DropTable {
            uid: t_uid.clone(),
            name: table_name.clone(),
            detach_from: None,
        }));
        guarded.drop_guard = Some(DropGuard {
            table_uid: t_uid,
            table: table_name,
            detach_from: None,
            requirements,
        });
        let guard_cs = ChangeSet {
            changes: vec![guarded],
        };
        let guard_ids = crate::plan_fixture::ids(&guard_cs);
        let guard_names =
            pbps_model::plan_names::PlanNames::of(&guard_cs.changes, &guard_ids).expect("names");
        let guard_err =
            now(dialect.vet_plan_sites(&state, &mut conn, &guard_names, &mut |_| {})).unwrap_err();
        assert!(
            guard_err.to_string().contains("SQLite")
                && guard_err.to_string().contains("drop-requirement"),
            "{guard_err}"
        );
    }
}
