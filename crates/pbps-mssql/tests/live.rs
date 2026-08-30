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
    ReferentialAction, Schema, Table, TableName, UniqueConstraint,
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
        for stmt in Mssql.emit(&p.change).expect("emit") {
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
