//! The SPEC §11.5 invariants, run against a real SQL Server.
//!
//! Everything below `assemble` is covered by unit tests; what only a live server
//! can verify is the other half: that the catalog queries return the shapes the
//! raw structs expect, that the emitter's SQL is SQL the engine accepts, and
//! that the two agree with each other — bootstrap then introspect must return
//! the declared schema, and applying a planned migration must converge on the
//! target.
//!
//! These tests are `#[ignore]`d because they need a server. Run them with
//! `scripts/live-tests.sh`, or start a container yourself and:
//!
//! ```text
//! export PBPS_TEST_DB='Server=localhost,14330;User Id=sa;Password=...;TrustServerCertificate=true'
//! cargo test -p pbps-mssql --test live -- --ignored
//! ```
//!
//! Each test creates and drops its own database, so they can run in parallel
//! and a failed run leaves debris only in a throwaway container.
//!
//! On expression text (defaults, checks, filters): the engine does not store
//! what the user wrote — `amount > 0` becomes `([amount]>(0))`. The declarations
//! in these tests use the engine's stored spelling so that equality holds; the
//! differ-side lightweight normalization for arbitrary spellings is a Phase 3
//! concern (SPEC §8, decision 9).

use pbps_db::Conn;
use pbps_dialect::Dialect;
use pbps_model::{
    Column, ColumnType, ForeignKey, Identity, IdsFile, Index, IndexColumn, Intent, PrimaryKey,
    ReferentialAction, Schema, StateSnapshot, Table, TableName, UniqueConstraint,
};
use pbps_mssql::Mssql;

fn conn_str() -> String {
    std::env::var("PBPS_TEST_DB").expect(
        "PBPS_TEST_DB is not set; these tests need a live SQL Server (see scripts/live-tests.sh)",
    )
}

/// A throwaway database that removes itself.
struct TestDb {
    name: String,
    conn: Conn,
}

impl TestDb {
    async fn create(tag: &str) -> TestDb {
        // The pid keeps two concurrent `cargo test` runs apart.
        let name = format!("pbps_test_{tag}_{}", std::process::id());
        let mut conn = Conn::connect(&conn_str()).await.expect("connect");
        conn.execute(&format!("CREATE DATABASE [{name}];"))
            .await
            .expect("create database");
        conn.execute(&format!("USE [{name}];")).await.expect("use");
        TestDb { name, conn }
    }

    async fn drop(mut self) {
        // Failure to clean up must not obscure the test's own verdict; the
        // container is throwaway anyway.
        let _ = self
            .conn
            .execute(&format!(
                "USE master; ALTER DATABASE [{0}] SET SINGLE_USER WITH ROLLBACK IMMEDIATE; DROP DATABASE [{0}];",
                self.name
            ))
            .await;
    }
}

/// Emits and executes every change of a plan, in plan order.
async fn apply(conn: &mut Conn, cs: &pbps_model::ChangeSet) {
    for p in &cs.changes {
        for stmt in Mssql.emit(&p.change, p.strategy).expect("emit") {
            conn.execute(&stmt.sql)
                .await
                .unwrap_or_else(|e| panic!("the engine rejected:\n{}\n{e}", stmt.sql));
        }
    }
}

/// The declared side normalized the way introspection reports it, so the two
/// can be compared with `==`.
fn normalized(schema: &Schema) -> Schema {
    let mut s = schema.clone();
    for table in s.tables.values_mut() {
        for col in table.columns.values_mut() {
            col.ty = Mssql.normalize_type(&col.ty).expect("normalize");
        }
    }
    s
}

fn ty(s: &str) -> ColumnType {
    s.parse().unwrap()
}

fn ctx() -> pbps_diff::Context {
    pbps_diff::Context {
        operator: "live-test".into(),
        today: "2026-08-31".into(),
    }
}

fn mint_ids(schema: &Schema, base: &IdsFile, intents: &[Intent]) -> IdsFile {
    pbps_diff::resolve(schema, base, intents, &ctx())
        .expect("resolve")
        .ids
}

fn plan(
    base_schema: &Schema,
    base_ids: &IdsFile,
    declared: &Schema,
    declared_ids: &IdsFile,
) -> pbps_model::ChangeSet {
    pbps_diff::diff(
        pbps_diff::Side {
            schema: base_schema,
            ids: base_ids,
        },
        pbps_diff::Side {
            schema: declared,
            ids: declared_ids,
        },
        &Mssql,
        &pbps_model::Hints::default(),
    )
    .expect("diff")
}

/// A schema using every construct the model can express.
fn rich_schema() -> Schema {
    let mut region = Table::default();
    region
        .columns
        .insert("region_id".into(), Column::new(ty("int")).not_null());
    region
        .columns
        .insert("name".into(), Column::new(ty("nvarchar(100)")).not_null());
    region.primary_key = Some(PrimaryKey {
        name: Some("pk_region".into()),
        columns: vec!["region_id".into()],
    });

    let mut customer = Table::default();
    let mut id = Column::new(ty("bigint")).not_null();
    id.identity = Some(Identity {
        seed: 1,
        increment: 1,
    });
    customer.columns.insert("id".into(), id);
    customer
        .columns
        .insert("email".into(), Column::new(ty("nvarchar(255)")));
    let mut status = Column::new(ty("tinyint")).not_null();
    status.default = Some("0".into());
    customer.columns.insert("status".into(), status);
    let mut amount = Column::new(ty("decimal(18,2)")).not_null();
    amount.default = Some("0.00".into());
    customer.columns.insert("amount".into(), amount);
    customer
        .columns
        .insert("notes".into(), Column::new(ty("nvarchar(max)")));
    customer.columns.insert(
        "created_at".into(),
        Column::new(ty("datetime2(3)")).not_null(),
    );
    customer
        .columns
        .insert("region_id".into(), Column::new(ty("int")));
    customer.primary_key = Some(PrimaryKey {
        name: Some("pk_customer".into()),
        columns: vec!["id".into()],
    });
    customer.unique.insert(
        "uq_customer_email".into(),
        UniqueConstraint {
            columns: vec!["email".into()],
        },
    );
    customer.foreign_keys.insert(
        "fk_customer_region".into(),
        ForeignKey {
            columns: vec!["region_id".into()],
            references_table: TableName::new("dbo", "region"),
            references_columns: vec!["region_id".into()],
            on_delete: ReferentialAction::SetNull,
            on_update: ReferentialAction::NoAction,
        },
    );
    customer.checks.insert(
        "ck_customer_amount".into(),
        pbps_model::CheckConstraint {
            // Stored spelling; see the module docs.
            expression: "[amount]>=(0)".into(),
        },
    );
    customer.indexes.insert(
        "ix_customer_region".into(),
        Index {
            columns: vec![
                IndexColumn {
                    name: "region_id".into(),
                    descending: false,
                },
                IndexColumn {
                    name: "created_at".into(),
                    descending: true,
                },
            ],
            include: vec!["status".into()],
            unique: false,
            filter: Some("[region_id] IS NOT NULL".into()),
        },
    );

    let mut schema = Schema::default();
    schema
        .tables
        .insert(TableName::new("dbo", "region"), region);
    schema
        .tables
        .insert(TableName::new("dbo", "customer"), customer);
    schema
}

/// SPEC §11.5 invariant 2: declarations -> bootstrap into an empty database ->
/// introspect -> equals the declared state. "The declarations are the database."
#[tokio::test]
#[ignore = "needs a live SQL Server; run scripts/live-tests.sh"]
async fn bootstrap_then_introspect_returns_the_declared_schema() {
    let declared = rich_schema();
    let ids = mint_ids(&declared, &IdsFile::default(), &[]);
    let cs = plan(&Schema::default(), &IdsFile::default(), &declared, &ids);

    let mut db = TestDb::create("bootstrap").await;
    apply(&mut db.conn, &cs).await;

    let pulled = pbps_mssql::catalog::introspect(&mut db.conn)
        .await
        .expect("introspect");
    db.drop().await;

    assert_eq!(pulled.warnings, Vec::<String>::new());
    assert_eq!(pulled.schema, normalized(&declared));
}

/// SPEC §11.5 invariant 3, the most important one: a database in state A,
/// apply `plan(A -> B)`, introspect, equals B. The migration includes a column
/// rename, a widening, a tightened nullability with a new default, a dropped
/// column and a new index — the moves a real MR makes.
#[tokio::test]
#[ignore = "needs a live SQL Server; run scripts/live-tests.sh"]
async fn applying_a_planned_migration_converges_on_the_target() {
    let a = rich_schema();
    let ids_a = mint_ids(&a, &IdsFile::default(), &[]);
    let bootstrap = plan(&Schema::default(), &IdsFile::default(), &a, &ids_a);

    let mut db = TestDb::create("converge").await;
    apply(&mut db.conn, &bootstrap).await;
    let state_a = pbps_mssql::catalog::introspect(&mut db.conn)
        .await
        .expect("introspect A");
    assert_eq!(
        state_a.schema,
        normalized(&a),
        "precondition: A bootstrapped"
    );

    // B: rename email -> contact_email, widen amount, drop notes, add a column
    // and an index, drop the old index.
    let mut b = a.clone();
    {
        let customer = b
            .tables
            .get_mut(&TableName::new("dbo", "customer"))
            .unwrap();
        let email = customer.columns.shift_remove("email").unwrap();
        customer.columns.insert("contact_email".into(), email);
        customer.unique.insert(
            "uq_customer_email".into(),
            UniqueConstraint {
                columns: vec!["contact_email".into()],
            },
        );
        customer.columns["amount"].ty = ty("decimal(19,4)");
        customer.columns.shift_remove("notes");
        customer
            .columns
            .insert("loyalty".into(), Column::new(ty("int")));
        customer.indexes.remove("ix_customer_region");
        customer.indexes.insert(
            "ix_customer_loyalty".into(),
            Index {
                columns: vec![IndexColumn {
                    name: "loyalty".into(),
                    descending: false,
                }],
                include: vec![],
                unique: false,
                filter: None,
            },
        );
    }
    let intents = [
        Intent::RenameColumn {
            table: TableName::new("dbo", "customer"),
            from: "email".into(),
            to: "contact_email".into(),
        },
        Intent::DropColumn {
            column: TableName::new("dbo", "customer").column("notes"),
            reason: "merged into the CRM".into(),
        },
    ];
    let ids_b = mint_ids(&b, &ids_a, &intents);

    // The plan is computed against the *introspected* A, exactly as Phase 3's
    // `plan --db` will do it — not against the declared A.
    let migration = plan(&state_a.schema, &ids_a, &b, &ids_b);
    assert!(
        migration
            .changes
            .iter()
            .any(|p| matches!(p.change, pbps_model::Change::RenameColumn { .. })),
        "the rename must plan as a rename, not a drop + add: {migration:?}"
    );

    apply(&mut db.conn, &migration).await;
    let state_b = pbps_mssql::catalog::introspect(&mut db.conn)
        .await
        .expect("introspect B");
    db.drop().await;

    assert_eq!(state_b.warnings, Vec::<String>::new());
    assert_eq!(state_b.schema, normalized(&b));

    // And having converged, the next plan must be empty — the fixpoint check.
    let ids_b2 = mint_ids(&b, &ids_b, &intents);
    let again = plan(&state_b.schema, &ids_b2, &b, &ids_b2);
    assert!(
        again.is_empty(),
        "the plan after convergence must be empty: {again:?}"
    );
}

/// What the model cannot express comes back as warnings, never silently — on
/// tables created by hand the way a legacy database would have them.
#[tokio::test]
#[ignore = "needs a live SQL Server; run scripts/live-tests.sh"]
async fn pull_warns_about_what_it_cannot_express() {
    let mut db = TestDb::create("warns").await;
    db.conn
        .execute(
            "CREATE TABLE dbo.legacy (
                 id int NOT NULL CONSTRAINT pk_legacy PRIMARY KEY,
                 price money NOT NULL,
                 qty int NOT NULL,
                 total AS (price * qty),
                 doc xml NULL
             );",
        )
        .await
        .expect("create legacy");
    db.conn
        .execute(
            "CREATE TABLE dbo.plain (id int NOT NULL);
             CREATE CLUSTERED INDEX cx_plain ON dbo.plain (id);",
        )
        .await
        .expect("create plain");

    let pulled = pbps_mssql::catalog::introspect(&mut db.conn)
        .await
        .expect("introspect");
    db.drop().await;

    let legacy = &pulled.schema.tables[&TableName::new("dbo", "legacy")];
    assert!(legacy.columns.contains_key("doc"), "xml is expressible");
    assert!(
        !legacy.columns.contains_key("total"),
        "the computed column must be left out, not misdeclared"
    );
    assert!(
        pulled
            .warnings
            .iter()
            .any(|w| w.contains("total") && w.contains("computed")),
        "{:?}",
        pulled.warnings
    );
    assert!(
        pulled
            .warnings
            .iter()
            .any(|w| w.contains("cx_plain") && w.contains("clustered")),
        "{:?}",
        pulled.warnings
    );
}

/// ADR-0002: modules are not managed, but a pull that does not even see them
/// tells the user the database is covered when half of it is not.
#[tokio::test]
#[ignore = "needs a live SQL Server; run scripts/live-tests.sh"]
async fn pull_reads_every_kind_of_module_back() {
    let mut db = TestDb::create("modules").await;
    db.conn
        .execute("CREATE TABLE dbo.t (id int NOT NULL, flag bit NULL);")
        .await
        .expect("create table");
    // Each in its own batch: CREATE VIEW and CREATE PROCEDURE must each begin
    // their own.
    for sql in [
        "CREATE VIEW dbo.v_active AS SELECT id FROM dbo.t WHERE flag = 1;",
        "CREATE PROCEDURE dbo.sp_touch AS SELECT 1;",
        "CREATE FUNCTION dbo.fn_double(@n int) RETURNS int AS BEGIN RETURN @n * 2 END;",
        "CREATE TRIGGER dbo.tr_t ON dbo.t AFTER INSERT AS SELECT 1;",
        // Two that cannot be managed, and must be reported rather than lost:
        // SCHEMABINDING has nowhere to live in the model, and an encrypted
        // module has no readable definition at all.
        "CREATE VIEW dbo.v_bound WITH SCHEMABINDING AS SELECT id FROM dbo.t;",
        "CREATE PROCEDURE dbo.sp_secret WITH ENCRYPTION AS SELECT 1;",
    ] {
        db.conn
            .execute(sql)
            .await
            .unwrap_or_else(|e| panic!("{sql}\n{e}"));
    }

    let pulled = pbps_mssql::catalog::introspect(&mut db.conn)
        .await
        .expect("introspect");
    db.drop().await;

    let mut managed: Vec<String> = pulled
        .schema
        .modules
        .keys()
        .map(ToString::to_string)
        .collect();
    managed.sort();
    assert_eq!(
        managed,
        ["dbo.fn_double", "dbo.sp_touch", "dbo.tr_t", "dbo.v_active"],
        "every readable module kind must come back managed"
    );

    // The body, not the whole CREATE: the prefix is the emitter's, and keeping
    // it would make every declaration compare unequal to the database it came
    // from.
    let view = &pulled.schema.modules[&TableName::new("dbo", "v_active")];
    assert_eq!(view.definition, "SELECT id FROM dbo.t WHERE flag = 1;");
    let trigger = &pulled.schema.modules[&TableName::new("dbo", "tr_t")];
    assert_eq!(trigger.on, Some(TableName::new("dbo", "t")));

    let unmanaged: Vec<&str> = pulled
        .unmanaged_modules
        .iter()
        .map(|m| m.name.as_str())
        .collect();
    assert_eq!(unmanaged, ["dbo.sp_secret", "dbo.v_bound"]);

    // The table itself still came through untouched.
    assert!(
        pulled
            .schema
            .tables
            .contains_key(&TableName::new("dbo", "t"))
    );
}

/// The round trip modules stand on (ADR-0002): what the emitter sends, the
/// engine stores, and introspection reads back must be the definition that was
/// declared — or every apply would be followed by drift, for ever.
#[tokio::test]
#[ignore = "needs a live SQL Server; run scripts/live-tests.sh"]
async fn a_module_applied_then_read_back_equals_what_was_declared() {
    let mut db = TestDb::create("module_roundtrip").await;
    db.conn
        .execute("CREATE TABLE dbo.customer (id int NOT NULL, legacy_code varchar(10) NULL);")
        .await
        .expect("create table");

    let mut declared = Schema::default();
    declared.tables.insert(
        TableName::new("dbo", "customer"),
        pbps_model::Table::default(),
    );
    let modules = [
        (
            "dbo.active_customer",
            pbps_model::ModuleKind::View,
            None,
            "SELECT id\nFROM dbo.customer\nWHERE legacy_code IS NULL",
        ),
        (
            "dbo.sp_touch_customer",
            pbps_model::ModuleKind::Procedure,
            None,
            "@id int\nAS\nUPDATE dbo.customer SET legacy_code = NULL WHERE id = @id;",
        ),
        (
            "dbo.fn_customer_count",
            pbps_model::ModuleKind::Function,
            None,
            "()\nRETURNS int\nAS\nBEGIN RETURN (SELECT COUNT(*) FROM dbo.customer); END",
        ),
        (
            "dbo.tr_customer_audit",
            pbps_model::ModuleKind::Trigger,
            Some("dbo.customer"),
            "AFTER INSERT\nAS SELECT 1;",
        ),
    ];
    for (name, kind, on, definition) in modules {
        declared.modules.insert(
            name.parse().unwrap(),
            pbps_model::Module {
                kind,
                description: None,
                on: on.map(|t| t.parse().unwrap()),
                definition: definition.to_owned(),
            },
        );
    }

    // Apply exactly what the emitter produces, statement by statement.
    for (name, module) in &declared.modules {
        let sql = pbps_mssql::emit::module_definition(name, module).expect("emit");
        db.conn
            .execute(&sql)
            .await
            .unwrap_or_else(|e| panic!("the engine rejected:\n{sql}\n{e}"));
    }

    let pulled = pbps_mssql::catalog::introspect(&mut db.conn)
        .await
        .expect("introspect");
    db.drop().await;

    assert_eq!(
        pulled.schema.modules, declared.modules,
        "the stored definition must read back as the declaration that produced it"
    );

    // And therefore the differ sees nothing: a module that came back different
    // would be re-stated on every plan, which is drift that never goes quiet.
    let ids = mint_ids(&declared, &IdsFile::default(), &[]);
    let cs = plan(&pulled.schema, &ids, &declared, &ids);
    assert!(
        cs.changes.iter().all(|c| c.change.module_name().is_none()),
        "no module change should be planned: {:?}",
        cs.changes
    );
}

// ---------------------------------------------------------------------------
// Phase 3: the ledger, the lock, the transaction promise, and the queries that
// only a real catalog can answer.
// ---------------------------------------------------------------------------

fn snapshot(kind: pbps_model::StateKind, schema: &Schema, ids: &IdsFile) -> StateSnapshot {
    StateSnapshot::new(kind, schema.clone(), ids.clone(), "live-test")
}

/// SPEC §8.1: the whole state goes in and comes back out unchanged. Everything
/// downstream — drift, the plan checksum, `status` — reads this row, so a
/// serialization that lost a field would make every one of them quietly wrong.
#[tokio::test]
#[ignore = "needs a live SQL Server; run scripts/live-tests.sh"]
async fn the_ledger_returns_exactly_what_was_recorded() {
    let mut db = TestDb::create("ledger").await;
    let schema = normalized(&rich_schema());
    let ids = mint_ids(&schema, &IdsFile::default(), &[]);

    // A database with no ledger must say so, not fail with "invalid object
    // name": the remedy is `baseline`, and only this distinction can print it.
    assert!(matches!(
        pbps_mssql::state::latest(&mut db.conn).await,
        Err(pbps_db::LedgerError::NotInitialized)
    ));

    let mut first = snapshot(pbps_model::StateKind::Baseline, &schema, &ids);
    first.reason = Some("adopting the database as it stands".into());
    first.git_sha = Some("bd4be74".into());
    let id = pbps_mssql::state::record(&mut db.conn, &first)
        .await
        .expect("record");

    let back = pbps_mssql::state::latest(&mut db.conn)
        .await
        .expect("latest")
        .expect("an entry");
    assert_eq!(back.id, id);
    assert_eq!(
        back.snapshot, first,
        "the snapshot must survive the round trip"
    );
    // The server's clock, formatted as ISO 8601 by the query.
    assert_eq!(back.applied_at.len(), 23, "{}", back.applied_at);
    assert!(back.applied_at.contains('T'), "{}", back.applied_at);

    // Recording again must append, never overwrite: the ledger is a history.
    let second = snapshot(pbps_model::StateKind::Apply, &Schema::default(), &ids);
    let second_id = pbps_mssql::state::record(&mut db.conn, &second)
        .await
        .expect("record again");
    assert!(second_id > id);
    assert_eq!(
        pbps_mssql::state::latest(&mut db.conn)
            .await
            .unwrap()
            .unwrap()
            .snapshot
            .kind,
        pbps_model::StateKind::Apply
    );

    let kept = pbps_mssql::state::prune(&mut db.conn, 1)
        .await
        .expect("prune");
    assert_eq!(kept, 1, "one old entry removed");
    let history = pbps_mssql::state::history(&mut db.conn, 10).await.unwrap();
    assert_eq!(history.len(), 1);
    assert_eq!(
        history[0].id, second_id,
        "the newest is the one that survives"
    );

    db.drop().await;
}

/// ADR-0003: a staged apply's checkpoint is what makes a mid-way failure
/// visible rather than mysterious, and what `--resume` starts from. It only
/// does that if the marker survives `state_json` — the `kind` column and the
/// progress have to come back exactly as they went in.
#[tokio::test]
#[ignore = "needs a live SQL Server; run scripts/live-tests.sh"]
async fn a_staged_checkpoint_survives_the_ledger() {
    let mut db = TestDb::create("staged").await;
    let schema = normalized(&rich_schema());
    let ids = mint_ids(&schema, &IdsFile::default(), &[]);

    let mut checkpoint = snapshot(pbps_model::StateKind::Staged, &schema, &ids);
    checkpoint.plan_checksum = Some("a".repeat(64));
    checkpoint.staged = Some(pbps_model::StagedProgress {
        completed: 1,
        total: 2,
        last_statement: "CREATE INDEX [ix_live] ON [dbo].[customer] ([email] ASC);".into(),
    });
    pbps_mssql::state::record(&mut db.conn, &checkpoint)
        .await
        .expect("record the checkpoint");

    let back = pbps_mssql::state::latest(&mut db.conn)
        .await
        .expect("latest")
        .expect("an entry");
    assert_eq!(back.snapshot, checkpoint);
    let progress = back.snapshot.staged.expect("the progress marker");
    assert!(!progress.is_finished());

    // The closing entry carries no marker, and its absence is what tells every
    // later command the environment is no longer mid-deployment.
    let done = snapshot(pbps_model::StateKind::Apply, &schema, &ids);
    pbps_mssql::state::record(&mut db.conn, &done)
        .await
        .expect("record the finish");
    assert!(
        pbps_mssql::state::latest(&mut db.conn)
            .await
            .unwrap()
            .unwrap()
            .snapshot
            .staged
            .is_none()
    );

    db.drop().await;
}

/// A cross-schema rename is two statements, and a staged apply checkpoints
/// between them. The name the table carries in that gap is what the checkpoint
/// has to record, and the emitter is the only thing that can say what it is
/// (`Statement::renames`) — so what it says had better be what the engine did.
///
/// Only a real server can answer that: a unit test would be comparing the
/// emitter against itself.
#[tokio::test]
#[ignore = "needs a live SQL Server; run scripts/live-tests.sh"]
async fn a_cross_schema_rename_stops_where_the_emitter_says_it_does() {
    let mut db = TestDb::create("rename_gap").await;
    db.conn
        .execute("CREATE SCHEMA [sales];")
        .await
        .expect("create the target schema");
    db.conn
        .execute("CREATE TABLE [dbo].[customer] ([id] int NOT NULL);")
        .await
        .expect("create the table");

    let statements = Mssql
        .emit(
            &pbps_model::Change::RenameTable {
                uid: "t_a9k2mq".parse().unwrap(),
                from: "dbo.customer".parse().unwrap(),
                to: "sales.client".parse().unwrap(),
            },
            pbps_model::Strategy::default(),
        )
        .expect("emit");
    assert_eq!(statements.len(), 2, "a cross-schema rename takes both");

    // Run only the first, exactly as a staged apply that stopped there would
    // have, and ask the catalog what the table is called now.
    db.conn
        .execute(&statements[0].sql)
        .await
        .expect("transfer the table");
    let pulled = pbps_mssql::catalog::introspect(&mut db.conn)
        .await
        .expect("introspect");
    let names: Vec<TableName> = pulled.schema.tables.keys().cloned().collect();

    let (from, to) = &statements[0].renames[0];
    assert_eq!(from, &"dbo.customer".parse::<TableName>().unwrap());
    assert_eq!(
        names,
        vec![to.clone()],
        "the emitter said the table would be at `{to}`; the catalog says `{names:?}`"
    );

    // And the second statement finishes the move it declared.
    db.conn
        .execute(&statements[1].sql)
        .await
        .expect("rename the table");
    let pulled = pbps_mssql::catalog::introspect(&mut db.conn)
        .await
        .expect("introspect");
    assert_eq!(
        pulled.schema.tables.keys().cloned().collect::<Vec<_>>(),
        vec![statements[1].renames[0].1.clone()]
    );

    db.drop().await;
}

/// SPEC §8.1: `__pbps_lock` stops two pipelines applying at once. The gate is
/// the insert itself, not a preceding read — a check-then-insert would let two
/// runners through the check together.
#[tokio::test]
#[ignore = "needs a live SQL Server; run scripts/live-tests.sh"]
async fn the_lock_admits_one_holder_at_a_time() {
    let mut db = TestDb::create("lock").await;

    pbps_mssql::state::lock(&mut db.conn, "runner-a")
        .await
        .expect("first lock");

    match pbps_mssql::state::lock(&mut db.conn, "runner-b").await {
        Err(pbps_db::LedgerError::Locked(info)) => {
            assert_eq!(info.locked_by, "runner-a", "the holder must be named");
            assert!(!info.locked_at.is_empty());
        }
        other => panic!("the second holder must be refused, got {other:?}"),
    }

    assert!(pbps_mssql::state::unlock(&mut db.conn).await.unwrap());
    assert!(
        !pbps_mssql::state::unlock(&mut db.conn).await.unwrap(),
        "releasing a lock nobody holds is not an error, but it is not a release either"
    );
    pbps_mssql::state::lock(&mut db.conn, "runner-b")
        .await
        .expect("the lock is free again");

    db.drop().await;
}

/// SPEC §7.5, the promise the whole apply model rests on: one plan, one
/// transaction, all or nothing. Only a real engine can show that a statement
/// failing halfway leaves nothing behind — and only with XACT_ABORT ON, without
/// which SQL Server would keep the transaction alive and let the earlier
/// statements commit.
#[tokio::test]
#[ignore = "needs a live SQL Server; run scripts/live-tests.sh"]
async fn a_failed_statement_rolls_the_whole_plan_back() {
    let mut db = TestDb::create("rollback").await;
    db.conn
        .execute("CREATE TABLE dbo.t (id int NOT NULL);")
        .await
        .expect("create");

    db.conn.begin().await.expect("begin");
    db.conn
        .execute("ALTER TABLE dbo.t ADD good nvarchar(10) NULL;")
        .await
        .expect("the first statement succeeds");
    assert!(
        db.conn
            .execute("ALTER TABLE dbo.t ADD bad nosuchtype;")
            .await
            .is_err(),
        "the second statement must fail"
    );
    db.conn.rollback().await.expect("rollback");

    let pulled = pbps_mssql::catalog::introspect(&mut db.conn)
        .await
        .expect("introspect");
    let t = &pulled.schema.tables[&TableName::new("dbo", "t")];
    assert!(
        !t.columns.contains_key("good"),
        "the successful statement must have gone too: {:?}",
        t.columns.keys().collect::<Vec<_>>()
    );

    db.drop().await;
}

/// SPEC §7.4: a SCHEMABINDING view blocks a rename outright, and the report has
/// to say so before anything runs rather than letting the engine refuse
/// mid-apply. The dependency queries are the half no unit test can cover.
#[tokio::test]
#[ignore = "needs a live SQL Server; run scripts/live-tests.sh"]
async fn rename_impact_finds_the_dependencies_that_block() {
    use pbps_mssql::impact::{RenameTarget, rename_impact};

    let mut db = TestDb::create("impact").await;
    db.conn
        .execute(
            "CREATE TABLE dbo.customer (
                 id int NOT NULL CONSTRAINT pk_customer PRIMARY KEY,
                 email nvarchar(255) NULL,
                 amount decimal(18,2) NOT NULL
                     CONSTRAINT ck_customer_amount CHECK (amount >= 0)
             );",
        )
        .await
        .expect("create table");
    for sql in [
        "CREATE VIEW dbo.v_plain AS SELECT id, email FROM dbo.customer;",
        "CREATE VIEW dbo.v_bound WITH SCHEMABINDING AS SELECT id, email FROM dbo.customer;",
        "CREATE INDEX ix_customer_email ON dbo.customer (email);",
    ] {
        db.conn
            .execute(sql)
            .await
            .unwrap_or_else(|e| panic!("{sql}\n{e}"));
    }

    let target = RenameTarget::Column("dbo.customer.email".parse().unwrap());
    let report = rename_impact(&mut db.conn, &target).await.expect("impact");

    assert_eq!(
        report
            .blocking
            .iter()
            .map(|r| r.name.as_str())
            .collect::<Vec<_>>(),
        ["dbo.v_bound"],
        "only the schema-bound view blocks: {report:?}"
    );
    assert!(
        report.advisory.iter().any(|r| r.name == "dbo.v_plain"),
        "the ordinary view still breaks and must be listed: {report:?}"
    );
    assert!(
        report
            .advisory
            .iter()
            .any(|r| r.name == "ix_customer_email"),
        "an index whose name embeds the column is naming drift: {report:?}"
    );
    // The check names `amount`, not `email`; a report that flagged it would be
    // a report nobody reads.
    assert!(
        !report
            .advisory
            .iter()
            .any(|r| r.name == "ck_customer_amount"),
        "unrelated constraints must not be reported: {report:?}"
    );

    db.drop().await;
}

/// SPEC §7.5: the probes have to count what the engine would actually refuse.
/// Their whole value is the number they report, and only real rows can show
/// that the number is right.
#[tokio::test]
#[ignore = "needs a live SQL Server; run scripts/live-tests.sh"]
async fn preflight_probes_count_what_the_engine_would_refuse() {
    use pbps_dialect::Dialect;
    use pbps_model::{Change, ChangeSet, PlannedChange};

    let mut db = TestDb::create("probes").await;
    db.conn
        .execute(
            "CREATE TABLE dbo.region (region_id int NOT NULL CONSTRAINT pk_region PRIMARY KEY);
             CREATE TABLE dbo.customer (
                 id int NOT NULL,
                 email nvarchar(255) NULL,
                 region_id int NULL,
                 amount decimal(18,2) NOT NULL
             );",
        )
        .await
        .expect("create");
    db.conn
        .execute(
            "INSERT INTO dbo.region VALUES (1);
             INSERT INTO dbo.customer VALUES
                 (1, 'a@example.com', 1, 10.00),
                 (2, NULL,            1, -5.00),
                 (3, NULL,            99, 20.00),
                 (1, 'a@example.com', 1, 30.00);",
        )
        .await
        .expect("insert");

    let cs = ChangeSet {
        changes: vec![
            PlannedChange::new(Change::AlterColumnNullability {
                uid: "c_aaaaaa".parse().unwrap(),
                column: "dbo.customer.email".parse().unwrap(),
                ty: ty("nvarchar(255)"),
                to_nullable: false,
            }),
            PlannedChange::new(Change::AddCheck {
                table: TableName::new("dbo", "customer"),
                name: "ck_customer_amount".into(),
                constraint: pbps_model::CheckConstraint {
                    expression: "[amount] >= 0".into(),
                },
            }),
            PlannedChange::new(Change::AddForeignKey {
                table: TableName::new("dbo", "customer"),
                name: "fk_customer_region".into(),
                constraint: Box::new(ForeignKey {
                    columns: vec!["region_id".into()],
                    references_table: TableName::new("dbo", "region"),
                    references_columns: vec!["region_id".into()],
                    on_delete: ReferentialAction::NoAction,
                    on_update: ReferentialAction::NoAction,
                }),
            }),
            PlannedChange::new(Change::AddUnique {
                table: TableName::new("dbo", "customer"),
                name: "uq_customer_id".into(),
                constraint: UniqueConstraint {
                    columns: vec!["id".into()],
                },
            }),
        ],
    };

    let mut counts = Vec::new();
    for probe in Mssql.preflight(&cs) {
        let rows = db
            .conn
            .query(&probe.sql)
            .await
            .unwrap_or_else(|e| panic!("the engine rejected a probe:\n{}\n{e}", probe.sql));
        let n: i32 = rows[0].try_get_at(0).unwrap().unwrap();
        counts.push((probe.description, n));
    }
    db.drop().await;

    let by = |needle: &str| {
        counts
            .iter()
            .find(|(d, _)| d.contains(needle))
            .unwrap_or_else(|| panic!("no probe mentioning `{needle}` in {counts:?}"))
            .1
    };
    assert_eq!(by("NULLs"), 2, "two rows have a NULL email: {counts:?}");
    assert_eq!(by("check"), 1, "one row has a negative amount: {counts:?}");
    assert_eq!(by("parent"), 1, "one row points at region 99: {counts:?}");
    // Rows, not groups: two rows share id 1.
    assert_eq!(by("collide"), 2, "{counts:?}");
}

/// The permission check against a real least-privilege login.
///
/// This is the shape the check exists for and the shape no unit test can
/// verify: `sa` holds `CONTROL` and short-circuits the whole list, which is how
/// three permission bugs survived the first live test. It is also the only way
/// to find out whether `HAS_PERMS_BY_NAME` really answers the question — that
/// a grant on a *schema* satisfies a requirement the earlier version asked for
/// on the database, and reported as missing.
#[tokio::test]
#[ignore = "needs a live SQL Server; set PBPS_TEST_DB (see scripts/live-tests.sh)"]
async fn a_schema_scoped_grant_satisfies_the_readiness_check() {
    let mut db = TestDb::create("doctorperm").await;
    let login = format!("pbps_lp_{}", std::process::id());
    // Not a secret: this login exists for the length of one test inside a
    // throwaway container, and it is dropped below.
    let password = "pbpsLeastPrivilege!1";

    db.conn
        .execute(&format!(
            "USE master; \
             IF SUSER_ID('{login}') IS NOT NULL DROP LOGIN [{login}]; \
             CREATE LOGIN [{login}] WITH PASSWORD = '{password}', CHECK_POLICY = OFF;"
        ))
        .await
        .expect("create login");
    // Exactly what a careful DBA would grant, and nothing more: the schema-scoped
    // permissions on the schema, the four CREATEs at the database (SQL Server
    // will not grant them lower), and no database-wide ALTER or SELECT at all.
    db.conn
        .execute(&format!(
            "USE [{0}]; \
             CREATE USER [{login}] FOR LOGIN [{login}]; \
             GRANT VIEW DEFINITION, SELECT, INSERT, DELETE, ALTER, REFERENCES \
             ON SCHEMA::dbo TO [{login}]; \
             GRANT CREATE TABLE, CREATE VIEW, CREATE PROCEDURE, CREATE FUNCTION TO [{login}];",
            db.name
        ))
        .await
        .expect("grant");

    let base = conn_str();
    let as_login = base
        .split(';')
        .filter(|p| {
            let k = p
                .split('=')
                .next()
                .unwrap_or("")
                .trim()
                .to_ascii_lowercase();
            !matches!(
                k.as_str(),
                "user id" | "uid" | "password" | "pwd" | "database"
            )
        })
        .collect::<Vec<_>>()
        .join(";");
    let as_login = format!(
        "{as_login};User Id={login};Password={password};Database={}",
        db.name
    );

    let mut lp = Conn::connect(&as_login)
        .await
        .expect("connect as the login");
    let held = pbps_mssql::doctor::permissions(&mut lp, &["dbo".to_owned()])
        .await
        .expect("read permissions");

    // The point of the whole change: none of these are held on the *database*,
    // which is what the first version of this check asked about.
    assert!(
        !held.database.contains("ALTER"),
        "the test's own premise is wrong if this login has database-wide ALTER: {held:?}"
    );
    assert!(!held.database.contains("CONTROL"), "{held:?}");
    let gaps = pbps_mssql::doctor::missing(&held);
    assert!(
        gaps.is_empty(),
        "a correctly granted least-privilege login was reported as missing: {gaps:?}"
    );

    // The negative case, and the dangerous one: it can take the deployment lock
    // but not release it. `apply` would commit the schema change and only then
    // fail, leaving a stale lock for the next pipeline.
    db.conn
        .execute(&format!(
            "USE [{}]; REVOKE DELETE ON SCHEMA::dbo FROM [{login}];",
            db.name
        ))
        .await
        .expect("revoke");
    let mut lp = Conn::connect(&as_login).await.expect("reconnect");
    let held = pbps_mssql::doctor::permissions(&mut lp, &["dbo".to_owned()])
        .await
        .expect("read permissions");
    let gaps = pbps_mssql::doctor::missing(&held);
    assert_eq!(gaps.len(), 1, "{gaps:?}");
    assert_eq!(gaps[0].permission, "DELETE");
    assert_eq!(gaps[0].securable(), "SCHEMA::dbo");

    // And the narrowest shape of all: the ledger tables exist, and INSERT and
    // DELETE are granted on *those two objects* rather than on the schema. Only
    // an object-scope question can see that grant; a schema-scope one reports
    // it missing, which is the same over-demand at one level further down.
    // Before the ledger exists, creating it needs ALTER on its schema on top of
    // the database-level CREATE TABLE. This is the case a `doctor` that said
    // "ready" would strand at the first `ensure_tables`, so it is checked
    // against a real server rather than reasoned about.
    db.conn
        .execute(&format!(
            "USE [{}]; REVOKE ALTER ON SCHEMA::dbo FROM [{login}];",
            db.name
        ))
        .await
        .expect("revoke alter");
    let mut lp = Conn::connect(&as_login).await.expect("reconnect");
    // As a project that manages `app`, so the gap can only be the ledger's own
    // creation requirement and not the ordinary managed-schema `ALTER`.
    let held = pbps_mssql::doctor::permissions(&mut lp, &["app".to_owned()])
        .await
        .expect("read permissions");
    let gaps = pbps_mssql::doctor::missing(&held);
    assert!(
        gaps.iter()
            .any(|g| g.permission == "ALTER" && g.securable() == "SCHEMA::dbo"),
        "an account that cannot create the ledger must not pass readiness: {gaps:?}"
    );
    db.conn
        .execute(&format!(
            "USE [{}]; GRANT ALTER ON SCHEMA::dbo TO [{login}];",
            db.name
        ))
        .await
        .expect("restore alter");

    pbps_mssql::state::ensure_tables(&mut db.conn)
        .await
        .expect("create the ledger");
    db.conn
        .execute(&format!(
            "USE [{0}]; \
             REVOKE INSERT, DELETE ON SCHEMA::dbo FROM [{login}]; \
             GRANT INSERT, DELETE ON OBJECT::dbo.__pbps_state TO [{login}]; \
             GRANT INSERT, DELETE ON OBJECT::dbo.__pbps_lock TO [{login}];",
            db.name
        ))
        .await
        .expect("grant on the ledger objects");
    let mut lp = Conn::connect(&as_login).await.expect("reconnect");
    let held = pbps_mssql::doctor::permissions(&mut lp, &["dbo".to_owned()])
        .await
        .expect("read permissions");
    assert!(
        !held.schemas["dbo"].contains("INSERT"),
        "the premise is wrong if the schema grant survived the revoke: {held:?}"
    );
    let gaps = pbps_mssql::doctor::missing(&held);
    assert!(
        gaps.is_empty(),
        "a grant on the ledger objects alone was reported as missing: {gaps:?}"
    );

    // And once the ledger exists, the creation permission is spent: writing rows
    // needs INSERT and DELETE, not ALTER. Asked as a project that manages `app`
    // rather than `dbo` — with `dbo` managed, ALTER there is required for the
    // ordinary reason and this would say nothing.
    db.conn
        .execute(&format!(
            "USE [{}]; REVOKE ALTER ON SCHEMA::dbo FROM [{login}];",
            db.name
        ))
        .await
        .expect("revoke alter again");
    let mut lp = Conn::connect(&as_login).await.expect("reconnect");
    let held = pbps_mssql::doctor::permissions(&mut lp, &["app".to_owned()])
        .await
        .expect("read permissions");
    let gaps = pbps_mssql::doctor::missing(&held);
    assert!(
        !gaps.iter().any(|g| g.permission == "ALTER"),
        "ALTER on the ledger schema must not be demanded once it exists: {gaps:?}"
    );

    drop(lp);
    let name = db.name.clone();
    db.drop().await;
    let mut admin = Conn::connect(&conn_str()).await.expect("connect");
    let _ = admin
        .execute(&format!(
            "USE master; IF SUSER_ID('{login}') IS NOT NULL DROP LOGIN [{login}];"
        ))
        .await;
    let _ = name;
}
