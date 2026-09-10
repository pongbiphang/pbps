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

/// Opens a connection to the live SQL Server this suite runs against.
///
/// The driver is named here, once, rather than at each call site below: every
/// test in this file speaks to the container `scripts/live-tests.sh` starts,
/// and repeating that fact at each of them would say nothing the file header
/// does not already say. The PostgreSQL live suite is `pbps-pg`'s own, started
/// by `scripts/live-tests-pg.sh`.
async fn connect_live(connection: &str) -> Result<pbps_db::Conn, pbps_db::DbError> {
    pbps_db::Conn::connect(pbps_db::Driver::Mssql, connection).await
}

fn conn_str() -> String {
    std::env::var("PBPS_TEST_DB").expect(
        "PBPS_TEST_DB is not set; these tests need a live SQL Server (see scripts/live-tests.sh)",
    )
}

#[tokio::test]
#[ignore = "needs live SQL Server"]
async fn signed_defaults_are_comparable_before_and_after_catalog_folding() {
    let mut db = TestDb::create("signed_defaults").await;
    for (index, (default, stored, value)) in [
        ("- 1", "((-1))", "-1"),
        ("+ 1", "((1))", "1"),
        ("-(-1)", "((1))", "1"),
        ("- /* c */ 1", "((-1))", "-1"),
        ("(-CAST('1' AS int))", "( -CONVERT([int],'1'))", "-1"),
        ("(-CAST('1'\tAS\tint))", "( -CONVERT([int],'1'))", "-1"),
        ("(-CAST('1'\nAS\nint))", "( -CONVERT([int],'1'))", "-1"),
        ("(-CONVERT(int,'1'))", "( -CONVERT([int],'1'))", "-1"),
        (
            "(-CAST('1.25' AS decimal(10,2)))",
            "( -CONVERT([decimal](10,2),'1.25'))",
            "-1.25",
        ),
        (
            "(-CONVERT(numeric(10,2),'1.25'))",
            "( -CONVERT([numeric](10,2),'1.25'))",
            "-1.25",
        ),
    ]
    .into_iter()
    .enumerate()
    {
        let column_type = if value == "-1.25" {
            "decimal(10,2)"
        } else {
            "int"
        };
        db.conn.execute(&format!("CREATE TABLE dbo.t{index} (n {column_type} DEFAULT {default}); INSERT dbo.t{index} DEFAULT VALUES;")).await.unwrap();
        let rows = db.conn.query(&format!("SELECT definition FROM sys.default_constraints WHERE parent_object_id=OBJECT_ID('dbo.t{index}')")).await.unwrap();
        assert_eq!(rows[0].try_get::<&str>("definition").unwrap(), Some(stored));
        assert!(pbps_mssql::rows::is_constant(default), "declared {default}");
        assert!(pbps_mssql::rows::is_constant(stored), "stored {stored}");
        let rows = db
            .conn
            .query(&format!(
                "SELECT CONVERT(varchar(20),n) AS n FROM dbo.t{index} WHERE n=({default})"
            ))
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].try_get::<&str>("n").unwrap(), Some(value));
    }
    // Keys travel as quoted data, so comments and groupings are not SQL
    // trivia here. Even tab/LF differ from the ASCII spaces after a sign.
    for (key, accepted) in [
        ("- 1", true),
        ("+  1", true),
        ("-\t1", false),
        ("-\n1", false),
        ("- /* c */ 1", false),
        ("-(-1)", false),
        ("- -1", false),
    ] {
        let rows = db.conn.query(&format!("SELECT CONVERT(varchar(30),TRY_CONVERT(int,N'{key}')) AS i, CONVERT(varchar(30),TRY_CONVERT(decimal(10,2),N'{key}')) AS d")).await.unwrap();
        for column in ["i", "d"] {
            assert_eq!(
                rows[0].try_get::<&str>(column).unwrap().is_some(),
                accepted,
                "{key:?} as {column}"
            );
        }
    }
    db.drop().await;
}

#[tokio::test]
#[ignore = "needs live SQL Server"]
async fn throwing_signed_defaults_do_not_break_explicit_row_reads() {
    use pbps_model::{RowKey, RowScope};
    let mut db = TestDb::create("signed_cast_safety").await;
    for (i, (default, safe)) in [
        ("-CAST('abc' AS int)", false),
        ("-CAST('256' AS tinyint)", false),
        ("-CAST('255' AS tinyint)", true),
        ("-CAST('-32768' AS smallint)", false),
        ("+CAST('-32768' AS smallint)", true),
        ("-CAST('-9223372036854775808' AS bigint)", false),
        ("+CAST('-9223372036854775808' AS bigint)", true),
        ("-CAST(1.25 AS int)", true),
        ("-CAST('1.25' AS int)", false),
        ("-CAST(CAST(1.5 AS money) AS int)", true),
        ("-CAST('9.995' AS decimal(3,2))", false),
        ("-CAST('9.994' AS decimal(3,2))", true),
        ("-CAST('922337203685477.5807' AS money)", true),
        ("-CAST('922337203685477.5808' AS money)", false),
        ("-CAST('-922337203685477.5808' AS money)", false),
        ("+CAST('-922337203685477.5808' AS money)", true),
        ("-CAST('214748.3647' AS smallmoney)", true),
        ("-CAST('214748.3648' AS smallmoney)", false),
        ("-CAST('214748.36475' AS smallmoney)", false),
        ("-CAST('-214748.3648' AS smallmoney)", false),
        ("+CAST('-214748.3648' AS smallmoney)", true),
        ("-CAST('-0.00005' AS money)", true),
        ("-CAST('+ 1' AS float)", false),
        ("-CAST('+-1' AS float)", false),
        ("-CAST('--1' AS real)", false),
        ("-CAST('1e38' AS real)", true),
        ("-CAST('1e39' AS real)", false),
        ("-CAST('1e308' AS float)", true),
        ("-CAST('1e309' AS float)", false),
        ("-CAST('1e-400' AS float)", true),
        ("-CAST('1e-100' AS real)", true),
        // Valid engine conversions outside the proof's grammar stay unknown.
        ("-CAST(CAST('1.25' AS float) AS int)", false),
        ("-CAST('$1' AS money)", false),
    ]
    .into_iter()
    .enumerate()
    {
        let name = TableName::new("dbo", format!("t{i}"));
        let approximate = default.contains("float") || default.contains("real");
        let column_type = if approximate {
            "float"
        } else {
            "decimal(38,10)"
        };
        db.conn.execute(&format!("CREATE TABLE dbo.t{i} (id int PRIMARY KEY, n {column_type} DEFAULT ({default})); INSERT dbo.t{i}(id,n) VALUES(1,1);")).await.unwrap();
        let mut table = Table::default();
        table
            .columns
            .insert("id".into(), Column::new("int".parse().unwrap()).not_null());
        let mut column = Column::new(column_type.parse().unwrap());
        // The catalog deparse is the spelling that broke actual row reads.
        let defaults = db.conn.query(&format!("SELECT definition FROM sys.default_constraints WHERE parent_object_id=OBJECT_ID('dbo.t{i}')")).await.unwrap();
        column.default = Some(
            defaults[0]
                .try_get::<&str>("definition")
                .unwrap()
                .unwrap()
                .to_owned(),
        );
        table.columns.insert("n".into(), column);
        table.primary_key = Some(PrimaryKey {
            name: None,
            columns: vec!["id".into()],
        });
        let read = |key| {
            pbps_mssql::rows::query(
                &name,
                &table,
                &RowScope::Keys([RowKey::from(key)].into_iter().collect()),
            )
            .unwrap()
            .unwrap()
        };
        let query = read("1");
        let rows = db
            .conn
            .query(&query.sql)
            .await
            .unwrap_or_else(|e| panic!("explicit row with {default}: {e}"));
        let (_, observed) = pbps_mssql::rows::decode(&name, &query, &rows[0]).unwrap();
        assert_eq!(
            pbps_mssql::rows::confirms_default(&table.columns["n"]),
            safe,
            "{default}"
        );
        assert_eq!(observed.unknown.contains("n"), !safe, "{default}");
        if safe {
            db.conn
                .execute(&format!("INSERT dbo.t{i}(id) VALUES(2);"))
                .await
                .unwrap_or_else(|e| panic!("proved default {default}: {e}"));
            // The cast proof concerns evaluation, not float-to-text style 3.
            // Ask directly for approximate values: that existing formatter
            // can itself throw for an otherwise valid stored float.
            if approximate {
                let rows = db
                    .conn
                    .query(&format!(
                        "SELECT COUNT(*) AS n FROM dbo.t{i} WHERE id=2 AND n=({default})"
                    ))
                    .await
                    .unwrap();
                assert_eq!(rows[0].try_get::<i32>("n").unwrap(), Some(1), "{default}");
                continue;
            }
            let query = read("2");
            let rows = db
                .conn
                .query(&query.sql)
                .await
                .unwrap_or_else(|e| panic!("default row with {default}: {e}; {}", query.sql));
            let (_, observed) = pbps_mssql::rows::decode(&name, &query, &rows[0]).unwrap();
            assert!(observed.at_default.contains("n"), "{default}");
        }
    }
    db.drop().await;
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
        let mut conn = connect_live(&conn_str()).await.expect("connect");
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

/// The same, in one transaction the way `apply` runs a plan: the engine's
/// refusal comes back with the statement, and nothing before it stays.
async fn try_apply(conn: &mut Conn, cs: &pbps_model::ChangeSet) -> Result<(), String> {
    conn.begin(Mssql.transaction_framing())
        .await
        .expect("begin");
    for p in &cs.changes {
        for stmt in Mssql.emit(&p.change, p.strategy).expect("emit") {
            if let Err(e) = conn.execute(&stmt.sql).await {
                conn.rollback(Mssql.transaction_framing())
                    .await
                    .expect("rollback");
                return Err(format!("{}\n{e}", stmt.sql));
            }
        }
    }
    conn.commit(Mssql.transaction_framing())
        .await
        .expect("commit");
    Ok(())
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

/// A columnstore or XML index has the same catalog shape as a rowstore one and
/// is not `type = 1`, so the clustered flag alone called it an ordinary index
/// and `pull` wrote it into the declarations as one — bootstrapping a B-tree
/// where the database had a columnstore, or a `CREATE INDEX` the engine cannot
/// run. The engine is asked here because the shape of the catalog rows these
/// indexes produce is the whole question (issue #89).
#[tokio::test]
#[ignore = "needs a live SQL Server; run scripts/live-tests.sh"]
async fn an_index_whose_physical_kind_the_model_cannot_hold_is_never_adopted_as_a_plain_one() {
    let mut db = TestDb::create("kinds").await;
    db.conn
        .execute(
            "CREATE TABLE dbo.wide (id int NOT NULL, note nvarchar(50) NULL);
             CREATE NONCLUSTERED COLUMNSTORE INDEX ncci_wide ON dbo.wide (id, note);
             CREATE INDEX ix_wide_note ON dbo.wide (note);",
        )
        .await
        .expect("create wide");
    db.conn
        .execute(
            "CREATE TABLE dbo.archive (id int NOT NULL, note nvarchar(50) NULL);
             CREATE CLUSTERED COLUMNSTORE INDEX cci_archive ON dbo.archive;",
        )
        .await
        .expect("create archive");
    db.conn
        .execute(
            "CREATE TABLE dbo.docs (
                 id int NOT NULL CONSTRAINT pk_docs PRIMARY KEY CLUSTERED,
                 doc xml NULL
             );
             CREATE PRIMARY XML INDEX pxi_docs ON dbo.docs (doc);",
        )
        .await
        .expect("create docs");

    let pulled = pbps_mssql::catalog::introspect(&mut db.conn)
        .await
        .expect("introspect");
    db.drop().await;

    let indexes = |schema: &str, table: &str| {
        pulled.schema.tables[&TableName::new(schema, table)]
            .indexes
            .keys()
            .cloned()
            .collect::<Vec<_>>()
    };
    // The negative case: the one kind the model does hold is still adopted.
    assert_eq!(indexes("dbo", "wide"), ["ix_wide_note"]);
    assert!(indexes("dbo", "archive").is_empty());
    assert!(indexes("dbo", "docs").is_empty());

    let said = pulled.warnings.join("\n");
    for (name, kind) in [
        ("ncci_wide", "columnstore"),
        ("cci_archive", "columnstore"),
        ("pxi_docs", "XML"),
    ] {
        assert!(
            pulled
                .warnings
                .iter()
                .any(|w| w.contains(name) && w.contains(kind)),
            "{name} was not reported as {kind}: {said}"
        );
    }
}

/// A permission row whose joined securable name is NULL is reported, not
/// dropped (issue #93) — but a grant on a *system* object must not become one
/// of those: its `major_id` is absent from `sys.objects` and present in
/// `sys.all_objects`, so the narrower join called an ordinary
/// `GRANT SELECT ON sys.objects` unnameable, and a reported grant with no
/// target refuses every connected command. The engine is asked because which
/// catalog view holds the row is the whole question.
#[tokio::test]
#[ignore = "needs a live SQL Server; run scripts/live-tests.sh"]
async fn a_grant_on_a_system_object_is_reported_with_its_name_not_as_unnameable() {
    let mut db = TestDb::create("nameless").await;
    db.conn
        .execute(
            "CREATE TABLE dbo.customer (id int NOT NULL);
             CREATE ROLE app_reader;
             GRANT SELECT ON dbo.customer TO app_reader;
             GRANT SELECT ON OBJECT::sys.objects TO app_reader;",
        )
        .await
        .expect("create the role and its grants");

    let pulled = pbps_mssql::catalog::introspect(&mut db.conn)
        .await
        .expect("introspect");
    db.drop().await;

    let said = format!("{:?}", pulled.unexpressible);
    // The negative case: the ordinary table is adopted either way.
    let targets: Vec<String> = pulled.schema.roles["app_reader"]
        .grants
        .keys()
        .map(ToString::to_string)
        .collect();
    assert_eq!(targets, ["dbo.customer"], "{said}");
    // The system object is not a table pbps models, so the grant on it is
    // reported rather than adopted — but *with* its target, which is what the
    // managed-set filter needs to discard it as somebody else's object. Named
    // nowhere, it was a targetless report, and those refuse every command.
    let system_grant: Vec<&pbps_mssql::introspect::Unexpressible> = pulled
        .unexpressible
        .iter()
        .filter(|u| {
            u.target
                == Some(pbps_model::GrantTarget::Object(
                    pbps_model::ObjectName::new("sys", "objects"),
                ))
        })
        .collect();
    assert_eq!(system_grant.len(), 1, "{said}");
    assert!(
        !pulled
            .unexpressible
            .iter()
            .any(|u| u.what.contains("cannot name")),
        "a grant the catalog can name was reported as unnameable: {said}"
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
        [
            "dbo.fn_double",
            "dbo.sp_touch",
            "dbo.t.tr_t",
            "dbo.v_active"
        ],
        "every readable module kind must come back managed, the trigger under its table (ADR-0009 §1)"
    );

    // The body, not the whole CREATE: the prefix is the emitter's, and keeping
    // it would make every declaration compare unequal to the database it came
    // from.
    let view =
        &pulled.schema.modules[&pbps_model::ModuleId::Named(TableName::new("dbo", "v_active"))];
    assert_eq!(view.definition, "SELECT id FROM dbo.t WHERE flag = 1;");
    // The trigger's table is its key's, not a field beside it (ADR-0009 §1).
    let trigger_id = pbps_model::ModuleId::Trigger {
        on: TableName::new("dbo", "t"),
        name: "tr_t".to_owned(),
    };
    assert!(
        pulled.schema.modules.contains_key(&trigger_id),
        "a pulled trigger is keyed by its table: {:?}",
        pulled.schema.modules.keys().collect::<Vec<_>>()
    );

    let unmanaged: Vec<String> = pulled
        .unmanaged_modules
        .iter()
        .map(|m| m.name.to_string())
        .collect();
    assert_eq!(
        unmanaged,
        ["dbo.sp_secret".to_owned(), "dbo.v_bound".to_owned()]
    );

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
/// A bare name resolves in the view's own schema and then in `dbo`, and
/// nowhere else — measured, `a.x AS SELECT * FROM z` reads `dbo.z` with
/// `b.z` present and is refused with only `b.z` present. So the alias `x` in
/// `b.z` is no mention of `a.x`; read as one, it closed a cycle that created
/// `a.x` over a view that did not exist yet.
#[tokio::test]
#[ignore = "needs a live SQL Server; run scripts/live-tests.sh"]
async fn a_bare_name_in_another_schema_is_not_an_edge() {
    let mut db = TestDb::create("path_order").await;
    for schema in ["a", "b"] {
        db.conn
            .execute(&format!("CREATE SCHEMA {schema}"))
            .await
            .expect("a schema");
    }
    let mut declared = Schema::default();
    for (name, definition) in [("a.x", "SELECT * FROM b.z"), ("b.z", "SELECT 1 AS x")] {
        declared.modules.insert(
            pbps_model::ModuleId::Named(name.parse().unwrap()),
            pbps_model::Module {
                kind: pbps_model::ModuleKind::View,
                description: None,
                definition: definition.to_owned(),
            },
        );
    }
    let ids = mint_ids(&declared, &IdsFile::default(), &[]);
    let cs = plan(&Schema::default(), &IdsFile::default(), &declared, &ids);
    let created: Vec<String> = cs
        .changes
        .iter()
        .filter_map(|c| {
            if let pbps_model::Change::CreateModule { id, .. } = &c.change {
                Some(id.to_string())
            } else {
                None
            }
        })
        .collect();
    assert_eq!(
        created,
        vec!["b.z".to_owned(), "a.x".to_owned()],
        "the view that is selected from comes first, whatever its columns are called"
    );
    apply(&mut db.conn, &cs).await;
    let rows = db
        .conn
        .query("SELECT x FROM a.x")
        .await
        .expect("the view over the view");
    assert_eq!(rows.len(), 1);
    db.drop().await;
}

/// Measured: `FROM select` is refused and `FROM [select]` names the view, so
/// the keyword that opens every view mentions no view named `select`. Read
/// as a mention, it drew an edge from `z` to `select`; with `select`
/// selecting from `z`, a cycle, and `select` was created first — over a view
/// that did not exist yet.
#[tokio::test]
#[ignore = "needs a live SQL Server; run scripts/live-tests.sh"]
async fn a_view_named_by_a_reserved_word_is_created_after_the_view_it_selects_from() {
    let mut db = TestDb::create("reserved_order").await;
    let mut declared = Schema::default();
    for (name, definition) in [
        ("dbo.select", "SELECT * FROM dbo.z"),
        ("dbo.z", "select 1 AS x"),
    ] {
        declared.modules.insert(
            pbps_model::ModuleId::Named(name.parse().unwrap()),
            pbps_model::Module {
                kind: pbps_model::ModuleKind::View,
                description: None,
                definition: definition.to_owned(),
            },
        );
    }
    let ids = mint_ids(&declared, &IdsFile::default(), &[]);
    let cs = plan(&Schema::default(), &IdsFile::default(), &declared, &ids);
    let created: Vec<String> = cs
        .changes
        .iter()
        .filter_map(|c| {
            if let pbps_model::Change::CreateModule { id, .. } = &c.change {
                Some(id.to_string())
            } else {
                None
            }
        })
        .collect();
    assert_eq!(
        created,
        vec!["dbo.z".to_owned(), "dbo.select".to_owned()],
        "the view that is selected from comes first, whatever its keywords spell"
    );
    apply(&mut db.conn, &cs).await;
    let rows = db
        .conn
        .query("SELECT x FROM dbo.[select]")
        .await
        .expect("the view over the view");
    assert_eq!(rows.len(), 1);
    db.drop().await;
}

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
        let name: pbps_model::ObjectName = name.parse().unwrap();
        let id = match on {
            Some(table) => pbps_model::ModuleId::Trigger {
                on: table.parse().unwrap(),
                name: name.name.clone(),
            },
            None => pbps_model::ModuleId::Named(name),
        };
        declared.modules.insert(
            id,
            pbps_model::Module {
                kind,
                description: None,
                definition: definition.to_owned(),
            },
        );
    }

    // Apply exactly what the emitter produces, statement by statement.
    for (id, module) in &declared.modules {
        let sql = pbps_mssql::emit::module_definition(id, module).expect("emit");
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
        cs.changes.iter().all(|c| c.change.module_id().is_none()),
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

    db.conn
        .begin(Mssql.transaction_framing())
        .await
        .expect("begin");
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
    db.conn
        .rollback(Mssql.transaction_framing())
        .await
        .expect("rollback");

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

/// The question the offline tests cannot answer: which form `OBJECT_ID`
/// actually resolves.
///
/// Asserted **both ways** on the same table, because the fix rests on the bare
/// form being the broken one. A test of the quoted form alone would pass just
/// as well if `OBJECT_ID` accepted both, and then the bug it is pinning would
/// not exist and neither would the reason for the change.
///
/// The name carries an embedded **period**, not a space. `OBJECT_ID` splits its
/// argument on periods and is otherwise tolerant, so `N'dbo.cust omer'` finds
/// the table and a test built on a space would have passed with the fix
/// reverted — the shape of test this project keeps catching. A period makes
/// the bare form a three-part name, `dbo`.`cust`.`omer`, which names a
/// different object entirely.
#[tokio::test]
#[ignore = "needs a live SQL Server; run scripts/live-tests.sh"]
async fn object_id_resolves_a_quoted_name_and_not_the_bare_one() {
    let mut db = TestDb::create("object_id_form").await;
    db.conn
        .execute("CREATE TABLE dbo.[cust.omer] (id int NOT NULL);")
        .await
        .expect("create table");

    // Asked as `IS NULL`, so each answer is a plain `int` the seam already
    // reads rather than a nullable one.
    let quoted: i32 = db
        .conn
        .query("SELECT CASE WHEN OBJECT_ID(N'[dbo].[cust.omer]') IS NULL THEN 0 ELSE 1 END;")
        .await
        .expect("quoted")[0]
        .try_get_at(0)
        .unwrap()
        .unwrap();
    assert_eq!(
        quoted, 1,
        "the quoted form is the one the fix sends, and it has to resolve"
    );

    let bare: i32 = db
        .conn
        .query("SELECT CASE WHEN OBJECT_ID(N'dbo.cust.omer') IS NULL THEN 0 ELSE 1 END;")
        .await
        .expect("bare")[0]
        .try_get_at(0)
        .unwrap()
        .unwrap();
    assert_eq!(
        bare, 0,
        "the bare form resolving would mean this bug never existed"
    );

    db.drop().await;
}

/// The bug itself, end to end: a table whose name needs quoting still reports
/// the SCHEMABINDING view that blocks its rename.
///
/// Before the fix `OBJECT_ID` returned NULL here, every dependency query joined
/// against NULL, and the report came back **empty** — which reads as "nothing
/// depends on this". Empty is the dangerous answer, not a loud failure, which
/// is why this needs a real server and its own test.
///
/// The period again, and for the same reason: with a space in the name the
/// bare form still resolves, so this test would have passed against the code
/// it is meant to catch.
#[tokio::test]
#[ignore = "needs a live SQL Server; run scripts/live-tests.sh"]
async fn a_table_name_needing_quoting_still_reports_what_blocks_its_rename() {
    use pbps_mssql::impact::{RenameTarget, rename_impact};

    let mut db = TestDb::create("impact_quoted").await;
    db.conn
        .execute("CREATE TABLE dbo.[cust.omer] (id int NOT NULL, email nvarchar(255) NULL);")
        .await
        .expect("create table");
    db.conn
        .execute(
            "CREATE VIEW dbo.v_bound_odd WITH SCHEMABINDING \
             AS SELECT id, email FROM dbo.[cust.omer];",
        )
        .await
        .expect("create view");

    let target = RenameTarget::Table(pbps_model::TableName::new("dbo", "cust.omer"));
    let report = rename_impact(&mut db.conn, &target).await.expect("impact");

    assert!(
        !report.is_empty(),
        "an empty report here is the bug: it reads as `nothing depends on this`"
    );
    assert_eq!(
        report
            .blocking
            .iter()
            .map(|r| r.name.as_str())
            .collect::<Vec<_>>(),
        ["dbo.v_bound_odd"],
        "the schema-bound view blocks the rename and must be reported: {report:?}"
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

/// A character count cannot answer whether a Unicode value survives the
/// target code page, or whether it fits a UTF-8 target's byte bound. The
/// round-trip probe must count both before ALTER changes or refuses them.
#[tokio::test]
#[ignore = "needs a live SQL Server; run scripts/live-tests.sh"]
async fn unicode_to_varchar_probes_character_loss() {
    use pbps_model::{Change, ChangeSet, PlannedChange};

    let plan = |column: &str, from: &str, to: &str| ChangeSet {
        changes: vec![PlannedChange::new(Change::AlterColumnType {
            uid: "c_aaaaaa".parse().unwrap(),
            column: column.parse().unwrap(),
            from: ty(from),
            to: ty(to),
            from_nullable: true,
            to_nullable: true,
        })],
    };
    async fn counts(conn: &mut Conn, plan: &ChangeSet) -> std::collections::BTreeMap<String, i32> {
        let mut counts = std::collections::BTreeMap::new();
        for probe in Mssql.preflight(plan) {
            let rows = conn
                .query(&probe.sql)
                .await
                .unwrap_or_else(|e| panic!("the engine rejected a probe:\n{}\n{e}", probe.sql));
            counts.insert(probe.description, rows[0].try_get_at(0).unwrap().unwrap());
        }
        counts
    }
    let count = |counts: &std::collections::BTreeMap<String, i32>, needle: &str| {
        *counts
            .iter()
            .find(|(description, _)| description.contains(needle))
            .unwrap_or_else(|| panic!("no probe mentioning `{needle}` in {counts:?}"))
            .1
    };

    // The legacy database code page cannot represent the three characters.
    // They fit varchar(20), so the old LEN-only probe reported clean while
    // ALTER stored three question marks.
    let mut legacy = TestDb::create("unicode_narrow_legacy").await;
    legacy
        .conn
        .execute(
            "CREATE TABLE dbo.t (label nvarchar(20) NULL);\n\
             INSERT dbo.t VALUES (N'王小明');\n\
             CREATE TABLE dbo.old_text (label ntext NULL);\n\
             INSERT dbo.old_text VALUES (N'王小明');\n\
             CREATE TABLE dbo.max_text (label nvarchar(20) NULL);\n\
             INSERT dbo.max_text VALUES (N'王小明');\n\
             CREATE TABLE dbo.system_name (label sysname NULL);\n\
             INSERT dbo.system_name VALUES (N'王小明');\n\
             CREATE TABLE dbo.legacy_text_target (label nvarchar(20) NULL);\n\
             INSERT dbo.legacy_text_target VALUES (N'王小明');\n\
             CREATE TABLE dbo.alias_target (label nvarchar(20) NULL);\n\
             INSERT dbo.alias_target VALUES (N'王小明');\n\
             CREATE TABLE dbo.utf8_source (label varchar(20) COLLATE Latin1_General_100_CI_AS_SC_UTF8 NULL);\n\
             INSERT dbo.utf8_source VALUES (N'王小明');\n\
             CREATE TABLE dbo.utf8_widening (label varchar(20) COLLATE Latin1_General_100_CI_AS_SC_UTF8 NULL);\n\
             INSERT dbo.utf8_widening VALUES (N'王小明');\n\
             CREATE TABLE dbo.legacy_code_page_text (label text COLLATE Chinese_PRC_CI_AS NULL);\n\
             INSERT dbo.legacy_code_page_text VALUES (N'王小明');",
        )
        .await
        .unwrap();
    let measured = counts(
        &mut legacy.conn,
        &plan("dbo.t.label", "nvarchar(20)", "varchar(20)"),
    )
    .await;
    assert_eq!(count(&measured, "too long"), 0, "{measured:?}");
    assert_eq!(
        count(&measured, "changed when converted"),
        1,
        "{measured:?}"
    );
    legacy
        .conn
        .execute("ALTER TABLE dbo.t ALTER COLUMN label varchar(20) NULL;")
        .await
        .unwrap();
    let rows = legacy.conn.query("SELECT label FROM dbo.t;").await.unwrap();
    let changed: &str = rows[0].try_get_at(0).unwrap().unwrap();
    assert_eq!(changed, "???");
    let measured = counts(
        &mut legacy.conn,
        &plan("dbo.old_text.label", "ntext", "varchar(20)"),
    )
    .await;
    assert_eq!(count(&measured, "too long"), 0, "{measured:?}");
    assert_eq!(
        count(&measured, "changed when converted"),
        1,
        "{measured:?}"
    );
    legacy
        .conn
        .execute("ALTER TABLE dbo.old_text ALTER COLUMN label varchar(20) NULL;")
        .await
        .unwrap();
    let rows = legacy
        .conn
        .query("SELECT label FROM dbo.old_text;")
        .await
        .unwrap();
    let changed: &str = rows[0].try_get_at(0).unwrap().unwrap();
    assert_eq!(changed, "???");
    let measured = counts(
        &mut legacy.conn,
        &plan("dbo.max_text.label", "nvarchar(20)", "varchar(max)"),
    )
    .await;
    assert_eq!(measured.len(), 1, "{measured:?}");
    assert_eq!(
        count(&measured, "changed when converted"),
        1,
        "{measured:?}"
    );
    legacy
        .conn
        .execute("ALTER TABLE dbo.max_text ALTER COLUMN label varchar(max) NULL;")
        .await
        .unwrap();
    let rows = legacy
        .conn
        .query("SELECT label FROM dbo.max_text;")
        .await
        .unwrap();
    let changed: &str = rows[0].try_get_at(0).unwrap().unwrap();
    assert_eq!(changed, "???");
    let measured = counts(
        &mut legacy.conn,
        &plan("dbo.system_name.label", "sysname", "varchar(max)"),
    )
    .await;
    assert_eq!(measured.len(), 1, "{measured:?}");
    assert_eq!(
        count(&measured, "changed when converted"),
        1,
        "{measured:?}"
    );
    legacy
        .conn
        .execute("ALTER TABLE dbo.system_name ALTER COLUMN label varchar(max) NULL;")
        .await
        .unwrap();
    let rows = legacy
        .conn
        .query("SELECT label FROM dbo.system_name;")
        .await
        .unwrap();
    let changed: &str = rows[0].try_get_at(0).unwrap().unwrap();
    assert_eq!(changed, "???");
    let measured = counts(
        &mut legacy.conn,
        &plan("dbo.legacy_text_target.label", "nvarchar(20)", "text"),
    )
    .await;
    assert_eq!(measured.len(), 1, "{measured:?}");
    assert_eq!(
        count(&measured, "changed when converted"),
        1,
        "{measured:?}"
    );
    legacy
        .conn
        .execute("ALTER TABLE dbo.legacy_text_target ALTER COLUMN label text NULL;")
        .await
        .unwrap();
    let rows = legacy
        .conn
        .query("SELECT CONVERT(varchar(max), label) FROM dbo.legacy_text_target;")
        .await
        .unwrap();
    let changed: &str = rows[0].try_get_at(0).unwrap().unwrap();
    assert_eq!(changed, "???");
    let measured = counts(
        &mut legacy.conn,
        &plan(
            "dbo.alias_target.label",
            "nvarchar(20)",
            "character varying(max)",
        ),
    )
    .await;
    assert_eq!(measured.len(), 1, "{measured:?}");
    assert_eq!(
        count(&measured, "changed when converted"),
        1,
        "{measured:?}"
    );
    legacy
        .conn
        .execute("ALTER TABLE dbo.alias_target ALTER COLUMN label varchar(max) NULL;")
        .await
        .unwrap();
    let rows = legacy
        .conn
        .query("SELECT label FROM dbo.alias_target;")
        .await
        .unwrap();
    let changed: &str = rows[0].try_get_at(0).unwrap().unwrap();
    assert_eq!(changed, "???");
    let measured = counts(
        &mut legacy.conn,
        &plan("dbo.utf8_source.label", "varchar(20)", "varchar(10)"),
    )
    .await;
    assert_eq!(count(&measured, "too long"), 0, "{measured:?}");
    assert_eq!(
        count(&measured, "changed when converted"),
        1,
        "{measured:?}"
    );
    legacy
        .conn
        .execute("ALTER TABLE dbo.utf8_source ALTER COLUMN label varchar(10) NULL;")
        .await
        .unwrap();
    let rows = legacy
        .conn
        .query("SELECT label FROM dbo.utf8_source;")
        .await
        .unwrap();
    let changed: &str = rows[0].try_get_at(0).unwrap().unwrap();
    assert_eq!(changed, "???");
    let measured = counts(
        &mut legacy.conn,
        &plan("dbo.utf8_widening.label", "varchar(20)", "varchar(max)"),
    )
    .await;
    assert_eq!(measured.len(), 1, "{measured:?}");
    assert_eq!(
        count(&measured, "changed when converted"),
        1,
        "{measured:?}"
    );
    legacy
        .conn
        .execute("ALTER TABLE dbo.utf8_widening ALTER COLUMN label varchar(max) NULL;")
        .await
        .unwrap();
    let rows = legacy
        .conn
        .query("SELECT label FROM dbo.utf8_widening;")
        .await
        .unwrap();
    let changed: &str = rows[0].try_get_at(0).unwrap().unwrap();
    assert_eq!(changed, "???");
    let measured = counts(
        &mut legacy.conn,
        &plan("dbo.legacy_code_page_text.label", "text", "varchar(max)"),
    )
    .await;
    assert_eq!(measured.len(), 1, "{measured:?}");
    assert_eq!(
        count(&measured, "changed when converted"),
        1,
        "{measured:?}"
    );
    legacy
        .conn
        .execute("ALTER TABLE dbo.legacy_code_page_text ALTER COLUMN label varchar(max) NULL;")
        .await
        .unwrap();
    let rows = legacy
        .conn
        .query("SELECT label FROM dbo.legacy_code_page_text;")
        .await
        .unwrap();
    let changed: &str = rows[0].try_get_at(0).unwrap().unwrap();
    assert_eq!(changed, "???");
    legacy.drop().await;

    // Under a UTF-8 database default the characters survive, but varchar(4)
    // holds only the first three-byte character. LEN still answers 3 <= 4;
    // the round trip catches the byte truncation that makes ALTER refuse.
    let mut utf8 = TestDb::create("unicode_narrow_utf8").await;
    utf8.conn
        .execute("ALTER DATABASE CURRENT COLLATE Latin1_General_100_CI_AS_SC_UTF8;")
        .await
        .unwrap();
    utf8.conn
        .execute(
            "CREATE TABLE dbo.t (label nvarchar(20) NULL);\n\
             INSERT dbo.t VALUES (N'王小明');",
        )
        .await
        .unwrap();
    let measured = counts(
        &mut utf8.conn,
        &plan("dbo.t.label", "nvarchar(20)", "varchar(4)"),
    )
    .await;
    assert_eq!(count(&measured, "too long"), 0, "{measured:?}");
    assert_eq!(
        count(&measured, "changed when converted"),
        1,
        "{measured:?}"
    );
    let err = utf8
        .conn
        .execute("ALTER TABLE dbo.t ALTER COLUMN label varchar(4) NULL;")
        .await
        .unwrap_err();
    assert!(err.to_string().contains("2628"), "{err}");
    utf8.drop().await;
}

/// A unique index is a constraint, and a *filtered* one constrains only the
/// rows its predicate keeps. Both halves are claims about this engine, and
/// both decide whether a valid plan is refused, so the engine answers them:
/// the index that must fail is created and fails, and the one that must
/// succeed is created and succeeds.
///
/// Ignoring the filter would have reported the two duplicates below and
/// refused a plan SQL Server accepts without complaint.
#[tokio::test]
#[ignore = "needs a live SQL Server; run scripts/live-tests.sh"]
async fn a_unique_index_is_probed_and_a_filtered_one_only_over_the_rows_it_keeps() {
    use pbps_model::{Change, ChangeSet, PlannedChange};

    let mut db = TestDb::create("uniqidx").await;
    db.conn
        .execute(
            "CREATE TABLE dbo.customer (
                 id int NOT NULL,
                 email nvarchar(255) NULL,
                 deleted_at datetime2 NULL
             );
             INSERT INTO dbo.customer VALUES
                 (1, N'a@example.com', NULL),
                 (2, N'b@example.com', NULL),
                 (3, N'c@example.com', '2020-01-01'),
                 (4, N'c@example.com', '2020-01-01');",
        )
        .await
        .expect("create");

    let index = |unique: bool, filter: Option<&str>| Change::AddIndex {
        table: TableName::new("dbo", "customer"),
        name: "ix_customer_email".into(),
        index: Box::new(Index {
            columns: vec![IndexColumn {
                name: "email".into(),
                descending: false,
            }],
            include: Vec::new(),
            unique,
            filter: filter.map(str::to_owned),
        }),
    };
    let one = |change: Change| ChangeSet {
        changes: vec![PlannedChange::new(change)],
    };

    // A plain index constrains nothing, so there is nothing to count.
    assert!(
        Mssql.preflight(&one(index(false, None))).is_empty(),
        "a plain index is not probed"
    );

    let probes = Mssql.preflight(&one(index(true, None)));
    assert_eq!(probes.len(), 1, "{probes:?}");
    let rows = db
        .conn
        .query(&probes[0].sql)
        .await
        .unwrap_or_else(|e| panic!("the engine rejected a probe:\n{}\n{e}", probes[0].sql));
    let n: i32 = rows[0].try_get_at(0).unwrap().unwrap();
    // Rows, not groups: the two rows sharing c@example.com.
    assert_eq!(n, 2, "{}", probes[0].sql);

    let filtered = Mssql.preflight(&one(index(true, Some("[deleted_at] IS NULL"))));
    assert_eq!(filtered.len(), 1, "{filtered:?}");
    let rows = db
        .conn
        .query(&filtered[0].sql)
        .await
        .unwrap_or_else(|e| panic!("the engine rejected a probe:\n{}\n{e}", filtered[0].sql));
    let n: i32 = rows[0].try_get_at(0).unwrap().unwrap();
    assert_eq!(
        n, 0,
        "the duplicates are both deleted, so the filter exempts them: {}",
        filtered[0].sql
    );

    // What the engine does with the same two statements, which is the whole
    // reason either count is worth reporting.
    let refused = db
        .conn
        .execute("CREATE UNIQUE INDEX ix_a ON dbo.customer (email);")
        .await;
    assert!(
        refused.is_err(),
        "the unfiltered index must fail on the duplicates the probe counted"
    );
    db.conn
        .execute("CREATE UNIQUE INDEX ix_b ON dbo.customer (email) WHERE deleted_at IS NULL;")
        .await
        .expect("the filtered index exempts the deleted duplicates and is created");

    db.drop().await;
}

/// A filtered index's predicate is the user's text, and its literals are
/// coerced to the type the column has *when the query runs*. `AddIndex` runs
/// after `AlterColumnType`, so a probe taken before the retype asks a
/// different question from the one the engine will answer — and asks it in the
/// direction that refuses a valid plan.
///
/// Live because both halves are claims about this engine's comparison
/// precedence, and DECISIONS 152 is the record of the last time believing
/// those without measuring them was wrong.
#[tokio::test]
#[ignore = "needs a live SQL Server; run scripts/live-tests.sh"]
async fn a_filtered_predicate_reads_a_retyped_column_through_the_type_it_has_now() {
    use pbps_model::{Change, ChangeSet, PlannedChange};

    let mut db = TestDb::create("uniqidxretype").await;
    db.conn
        .execute(
            "CREATE TABLE dbo.customer (
                 id int NOT NULL,
                 email nvarchar(255) NULL,
                 flag int NULL
             );
             INSERT INTO dbo.customer VALUES
                 (1, N'a@example.com', 1),
                 (2, N'a@example.com', 1);",
        )
        .await
        .expect("create");

    // What the predicate keeps while `flag` is still `int`: the literal is
    // converted to `int`, so `1 = 1` and both duplicates are in the filtered
    // set. A probe taken now counts a collision.
    let rows = db
        .conn
        .query("SELECT COUNT(*) FROM dbo.customer WHERE flag = '01';")
        .await
        .expect("count over the stored type");
    let before: i32 = rows[0].try_get_at(0).unwrap().unwrap();
    assert_eq!(before, 2, "'01' is converted to int and matches flag = 1");

    db.conn
        .execute("ALTER TABLE dbo.customer ALTER COLUMN flag varchar(2);")
        .await
        .expect("retype");

    // What it keeps afterwards: string comparison, and `'1'` is not `'01'`.
    let rows = db
        .conn
        .query("SELECT COUNT(*) FROM dbo.customer WHERE flag = '01';")
        .await
        .expect("count over the new type");
    let after: i32 = rows[0].try_get_at(0).unwrap().unwrap();
    assert_eq!(
        after, 0,
        "'1' does not equal '01' once the column is varchar"
    );

    // So the plan is valid: the engine creates the index over no rows at all.
    db.conn
        .execute("CREATE UNIQUE INDEX ix_flagged ON dbo.customer (email) WHERE flag = '01';")
        .await
        .expect("the retyped column exempts both duplicates and the index is created");

    // Which is why the probe is not taken at all when the plan retypes a
    // column of the table: the count it could produce here is 2, and 2 refuses
    // the plan the engine just accepted.
    let probes = Mssql.preflight(&ChangeSet {
        changes: vec![
            PlannedChange::new(Change::AlterColumnType {
                uid: "c_bbbbbb".parse().unwrap(),
                column: TableName::new("dbo", "customer").column("flag"),
                from: ColumnType::simple("int"),
                to: ColumnType::new("varchar", vec![pbps_model::TypeArg::Int(2)]),
                from_nullable: true,
                to_nullable: true,
            }),
            PlannedChange::new(Change::AddIndex {
                table: TableName::new("dbo", "customer"),
                name: "ix_flagged".into(),
                index: Box::new(Index {
                    columns: vec![IndexColumn {
                        name: "email".into(),
                        descending: false,
                    }],
                    include: Vec::new(),
                    unique: true,
                    filter: Some("flag = '01'".into()),
                }),
            }),
        ],
    });
    assert!(
        !probes.iter().any(|p| p.description.contains("collide")),
        "{probes:?}"
    );

    db.drop().await;
}

/// `order_key` runs every row change before every constraint a plan adds, so
/// the rows a `UNIQUE` or a `PRIMARY KEY` will meet are the ones the plan
/// leaves. Counting the rows standing now refused a plan that deletes its own
/// duplicate first (DECISIONS 175).
///
/// Live because the fix is a claim about what this engine counts: the probes
/// now group over a derived table that subtracts the plan's deletes and unions
/// its inserts, and only a real server can say the number that comes back is
/// the number `ADD CONSTRAINT` would refuse.
#[tokio::test]
#[ignore = "needs a live SQL Server; run scripts/live-tests.sh"]
async fn a_key_probe_counts_the_rows_the_plan_will_leave() {
    use pbps_dialect::Dialect;
    use pbps_model::{Change, ChangeSet, PlannedChange};

    let mut db = TestDb::create("keyafter").await;
    db.conn
        .execute(
            "CREATE TABLE dbo.customer (code varchar(20) NOT NULL, email nvarchar(255) NULL);
             INSERT INTO dbo.customer VALUES ('keep', 'a@example.com'), ('dup', 'a@example.com');",
        )
        .await
        .expect("create");

    let unique = PlannedChange::new(Change::AddUnique {
        table: TableName::new("dbo", "customer"),
        name: "uq_email".into(),
        constraint: UniqueConstraint {
            columns: vec!["email".into()],
        },
    });
    let delete = PlannedChange::new(Change::DeleteRow {
        table: TableName::new("dbo", "customer"),
        key_column: "code".into(),
        key: pbps_model::RowKey::from("dup"),
        cause: pbps_model::change::DeleteCause::Undeclared,
        row: Default::default(),
        types: Default::default(),
        after_types: Default::default(),
    });

    let count = |changes: Vec<PlannedChange>| {
        let cs = ChangeSet { changes };
        Mssql
            .preflight(&cs)
            .into_iter()
            .find(|p| p.description.contains("collide"))
            .map(|p| p.sql)
    };
    let standing = count(vec![unique.clone()]).expect("a probe");
    let after_delete = count(vec![delete, unique]).expect("a probe");

    let mut counted = Vec::new();
    for sql in [&standing, &after_delete] {
        let rows = db
            .conn
            .query(sql)
            .await
            .unwrap_or_else(|e| panic!("the engine rejected a probe:\n{sql}\n{e}"));
        counted.push(rows[0].try_get_at::<i32>(0).unwrap().unwrap());
    }
    let (now, left) = (counted[0], counted[1]);
    db.drop().await;

    // Two rows share an address today, and the engine would refuse both.
    assert_eq!(now, 2, "{standing}");
    // The plan deletes one of them first, and then there is nothing to refuse.
    assert_eq!(left, 0, "{after_delete}");
}

/// A probe runs before the first statement, so a column the same plan is
/// *adding* is not there to be read — and every probe over one was
/// `Msg 207, Invalid column name`, which the runner reports as unchecked and
/// the apply then proceeds past (DECISIONS 124, 171).
///
/// Live because every claim in that fix is a claim about this engine: that
/// naming the column throws, that `ADD col NULL DEFAULT x` leaves the existing
/// rows NULL while `NOT NULL DEFAULT` backfills them, and that the counts the
/// substituted probes report are the ones the constraints would refuse. A unit
/// test can only check that the SQL says what I think it says.
#[tokio::test]
#[ignore = "needs a live SQL Server; run scripts/live-tests.sh"]
async fn probes_over_a_column_this_plan_adds_run_and_count_what_the_engine_refuses() {
    use pbps_dialect::Dialect;
    use pbps_model::{Change, ChangeSet, PlannedChange};

    let mut db = TestDb::create("addedcol").await;
    db.conn
        .execute(
            "CREATE TABLE dbo.region (region_id varchar(10) NOT NULL CONSTRAINT pk_r PRIMARY KEY);
             CREATE TABLE dbo.customer (id int NOT NULL);
             INSERT INTO dbo.region VALUES ('eu');
             INSERT INTO dbo.customer VALUES (1), (2), (3);",
        )
        .await
        .expect("create");

    let add = |nullable: bool, default: Option<&str>| {
        let mut column = pbps_model::Column::new(ty("varchar(10)"));
        column.nullable = nullable;
        column.default = default.map(str::to_owned);
        PlannedChange::new(Change::AddColumn {
            uid: "c_cccccc".parse().unwrap(),
            table: TableName::new("dbo", "customer"),
            name: "region_id".into(),
            column: Box::new(column),
        })
    };
    let unique = PlannedChange::new(Change::AddUnique {
        table: TableName::new("dbo", "customer"),
        name: "uq_region".into(),
        constraint: UniqueConstraint {
            columns: vec!["region_id".into()],
        },
    });
    let fk = PlannedChange::new(Change::AddForeignKey {
        table: TableName::new("dbo", "customer"),
        name: "fk_customer_region".into(),
        constraint: Box::new(ForeignKey {
            columns: vec!["region_id".into()],
            references_table: TableName::new("dbo", "region"),
            references_columns: vec!["region_id".into()],
            on_delete: ReferentialAction::NoAction,
            on_update: ReferentialAction::NoAction,
        }),
    });

    let mut counts = Vec::new();
    for (label, changes) in [
        // Nullable: the engine backfills nothing, so all three rows are NULL —
        // exempt from the foreign key, and one NULL too many for the unique.
        (
            "nullable",
            vec![add(true, Some("'zz'")), unique.clone(), fk.clone()],
        ),
        // NOT NULL with a constant default: all three rows are backfilled with
        // it, so all three collide and all three reference a real parent.
        (
            "backfilled",
            vec![add(false, Some("'eu'")), unique.clone(), fk.clone()],
        ),
        // The same, pointing at a parent row that is not there.
        ("orphaned", vec![add(false, Some("'us'")), unique, fk]),
    ] {
        for probe in Mssql.preflight(&ChangeSet { changes }) {
            let rows = db.conn.query(&probe.sql).await.unwrap_or_else(|e| {
                panic!("the engine rejected a probe ({label}):\n{}\n{e}", probe.sql)
            });
            let n: i32 = rows[0].try_get_at(0).unwrap().unwrap();
            counts.push((label, probe.description, n));
        }
    }
    db.drop().await;

    let by = |label: &str, needle: &str| {
        counts
            .iter()
            .find(|(l, d, _)| *l == label && d.contains(needle))
            .unwrap_or_else(|| panic!("no `{needle}` probe for {label} in {counts:?}"))
            .2
    };
    // Three rows share one value, whichever value it is: the unique refuses
    // all three, and `GROUP BY` cannot be asked — every row agrees.
    assert_eq!(by("nullable", "collide"), 3, "{counts:?}");
    assert_eq!(by("backfilled", "collide"), 3, "{counts:?}");
    // NULL references are the ones a foreign key exempts.
    assert_eq!(by("nullable", "parent"), 0, "{counts:?}");
    assert_eq!(by("backfilled", "parent"), 0, "{counts:?}");
    // And the backfilled value that has no parent row is three orphans.
    assert_eq!(by("orphaned", "parent"), 3, "{counts:?}");
}

/// The permission check against a real least-privilege login.
///
/// This is the shape the check exists for and the shape no unit test can
/// verify: `sa` holds `CONTROL`, which used to short-circuit the whole list —
/// that is how three permission bugs survived the first live test, and the
/// shortcut has since been removed for a fourth reason (see
/// `a_deny_beats_control_and_the_readiness_check_sees_it`). It is also the only way
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

    let mut lp = connect_live(&as_login).await.expect("connect as the login");
    let held = pbps_mssql::doctor::permissions(
        &mut lp,
        &["dbo".to_owned()],
        &[],
        &pbps_mssql::doctor::GrantTargets::default(),
        &pbps_mssql::doctor::DataTables::new(),
    )
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
    let mut lp = connect_live(&as_login).await.expect("reconnect");
    let held = pbps_mssql::doctor::permissions(
        &mut lp,
        &["dbo".to_owned()],
        &[],
        &pbps_mssql::doctor::GrantTargets::default(),
        &pbps_mssql::doctor::DataTables::new(),
    )
    .await
    .expect("read permissions");
    let gaps = pbps_mssql::doctor::missing(&held);
    assert_eq!(gaps.len(), 1, "{gaps:?}");
    assert_eq!(gaps[0].permission, "DELETE");
    assert_eq!(gaps[0].securable(), "SCHEMA::[dbo]");

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
    let mut lp = connect_live(&as_login).await.expect("reconnect");
    // As a project that manages `app`, so the gap can only be the ledger's own
    // creation requirement and not the ordinary managed-schema `ALTER`.
    let held = pbps_mssql::doctor::permissions(
        &mut lp,
        &["app".to_owned()],
        &[],
        &pbps_mssql::doctor::GrantTargets::default(),
        &pbps_mssql::doctor::DataTables::new(),
    )
    .await
    .expect("read permissions");
    let gaps = pbps_mssql::doctor::missing(&held);
    assert!(
        gaps.iter()
            .any(|g| g.permission == "ALTER" && g.securable() == "SCHEMA::[dbo]"),
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
    let mut lp = connect_live(&as_login).await.expect("reconnect");
    let held = pbps_mssql::doctor::permissions(
        &mut lp,
        &["dbo".to_owned()],
        &[],
        &pbps_mssql::doctor::GrantTargets::default(),
        &pbps_mssql::doctor::DataTables::new(),
    )
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

    // Half a ledger: one table dropped by hand, the other still carrying its
    // object grants. The scope has to be chosen per table — object where the
    // table is, schema where it is not — because the grant that will cover the
    // one `ensure_tables` is about to recreate can only be the schema's. Asking
    // once for the pair let this account pass readiness and then be denied on
    // the state read, so it is checked against a real server.
    db.conn
        .execute(&format!("USE [{0}]; DROP TABLE dbo.__pbps_state;", db.name))
        .await
        .expect("drop the state table");
    let mut lp = connect_live(&as_login).await.expect("reconnect");
    let held = pbps_mssql::doctor::permissions(
        &mut lp,
        &["dbo".to_owned()],
        &[],
        &pbps_mssql::doctor::GrantTargets::default(),
        &pbps_mssql::doctor::DataTables::new(),
    )
    .await
    .expect("read permissions");
    assert_eq!(
        held.ledger_objects.len(),
        1,
        "the premise: exactly one ledger table survives: {held:?}"
    );
    let gaps = pbps_mssql::doctor::missing(&held);
    // INSERT and DELETE only: this login still holds `SELECT ON SCHEMA::dbo`
    // from the top of the test, so the ledger read is genuinely satisfied and
    // reporting it would be the over-demand, not the fix. The unit test covers
    // all three by clearing the schema grants outright.
    for permission in ["INSERT", "DELETE"] {
        assert!(
            gaps.iter()
                .any(|g| g.permission == permission && g.securable() == "SCHEMA::[dbo]"),
            "{permission} for the table still to be created was not asked for: {gaps:?}"
        );
    }
    // The surviving table is still answered where its grant actually sits, or
    // the account would be told to re-grant what it already holds.
    assert!(
        !gaps
            .iter()
            .any(|g| g.securable() == "OBJECT::[dbo].[__pbps_lock]"),
        "{gaps:?}"
    );

    // Restored for the checks below: recreating the table also drops the object
    // grants that were on the old one.
    pbps_mssql::state::ensure_tables(&mut db.conn)
        .await
        .expect("recreate the ledger");
    db.conn
        .execute(&format!(
            "USE [{0}]; GRANT INSERT, DELETE ON OBJECT::dbo.__pbps_state TO [{login}];",
            db.name
        ))
        .await
        .expect("re-grant on the recreated state table");

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
    let mut lp = connect_live(&as_login).await.expect("reconnect");
    let held = pbps_mssql::doctor::permissions(
        &mut lp,
        &["app".to_owned()],
        &[],
        &pbps_mssql::doctor::GrantTargets::default(),
        &pbps_mssql::doctor::DataTables::new(),
    )
    .await
    .expect("read permissions");
    let gaps = pbps_mssql::doctor::missing(&held);
    assert!(
        !gaps.iter().any(|g| g.permission == "ALTER"),
        "ALTER on the ledger schema must not be demanded once it exists: {gaps:?}"
    );

    // A declared schema spelled differently from the catalog. On a
    // case-insensitive database `App` and `app` are the same schema, and the
    // server says so — but the answer used to come back under the *catalog's*
    // spelling, so the caller looked up the name it asked with and missed.
    // `doctor` then called an existing schema absent and advised creating it.
    // Its own batch: `CREATE SCHEMA` must be the first statement in one, so it
    // cannot share a batch with the `USE`.
    db.conn
        .execute(&format!("USE [{}];", db.name))
        .await
        .expect("use");
    db.conn
        .execute("CREATE SCHEMA [app];")
        .await
        .expect("create app schema");
    let held = pbps_mssql::doctor::permissions(
        &mut lp,
        &["App".to_owned()],
        &[],
        &pbps_mssql::doctor::GrantTargets::default(),
        &pbps_mssql::doctor::DataTables::new(),
    )
    .await
    .expect("read permissions");
    assert!(
        held.absent_schemas.is_empty(),
        "a schema that exists under another casing must not be reported absent: {held:?}"
    );
    assert!(
        held.schemas.contains_key("App"),
        "the answer must come back under the requested spelling: {held:?}"
    );

    // A declared schema the database does not have. Only a real `sys.schemas`
    // can answer this, and it is the case `doctor` used to pass in silence:
    // nothing in the tool emits `CREATE SCHEMA`, so the first
    // `CREATE TABLE [nowhere].[...]` would have failed right after a clean
    // readiness report.
    let held = pbps_mssql::doctor::permissions(
        &mut lp,
        &["nowhere".to_owned()],
        &[],
        &pbps_mssql::doctor::GrantTargets::default(),
        &pbps_mssql::doctor::DataTables::new(),
    )
    .await
    .expect("read permissions");
    assert!(
        held.absent_schemas.contains("nowhere"),
        "a declared schema the database lacks must be reported: {held:?}"
    );
    assert!(
        !held.absent_schemas.contains("dbo"),
        "the ledger's schema is not a declaration problem: {held:?}"
    );

    drop(lp);
    let name = db.name.clone();
    db.drop().await;
    let mut admin = connect_live(&conn_str()).await.expect("connect");
    let _ = admin
        .execute(&format!(
            "USE master; IF SUSER_ID('{login}') IS NOT NULL DROP LOGIN [{login}];"
        ))
        .await;
    let _ = name;
}

/// A declared `exact` table with one row and one correctable column, which is
/// what `app.t` below is: the demand travels through `DataDemand::of`, the one
/// place a declaration is read, rather than being hand-made here.
fn seeded_table() -> pbps_mssql::doctor::DataDemand {
    seeded_table_with(&["label"])
}

/// The same, with the correctable columns named — the set an emitted `UPDATE`
/// could reach, and therefore the set the permission has to cover.
fn seeded_table_with(columns: &[&str]) -> pbps_mssql::doctor::DataDemand {
    let ty: pbps_model::ColumnType = "varchar(20)".parse().unwrap();
    let mut t = pbps_model::schema::Table {
        primary_key: Some(pbps_model::schema::PrimaryKey {
            name: None,
            columns: vec!["code".to_owned()],
        }),
        data: Some(pbps_model::TableData {
            mode: pbps_model::DataMode::Exact,
            rows: [(
                pbps_model::RowKey("a".to_owned()),
                pbps_model::Row(Default::default()),
            )]
            .into_iter()
            .collect(),
        }),
        ..Default::default()
    };
    t.columns
        .insert("code".to_owned(), pbps_model::Column::new(ty.clone()));
    for column in columns {
        t.columns
            .insert((*column).to_owned(), pbps_model::Column::new(ty.clone()));
    }
    pbps_mssql::doctor::DataDemand::of(&t).expect("an `exact` table with a row demands all three")
}

/// The readiness question for one project's declared data tables, asked the
/// way `doctor` asks it.
async fn data_permissions(
    conn: &mut Conn,
    data: &pbps_mssql::doctor::DataTables,
) -> pbps_mssql::doctor::Held {
    pbps_mssql::doctor::permissions(
        conn,
        &["app".to_owned()],
        &[],
        &pbps_mssql::doctor::GrantTargets::default(),
        data,
    )
    .await
    .expect("read permissions")
}

/// The readiness question for one project's foreign-key targets outside the
/// managed schemas, asked the way `doctor` asks it.
async fn referenced_permissions(
    conn: &mut Conn,
    referenced: &[pbps_model::ObjectName],
) -> pbps_mssql::doctor::Held {
    pbps_mssql::doctor::permissions(
        conn,
        &["app".to_owned()],
        referenced,
        &pbps_mssql::doctor::GrantTargets::default(),
        &pbps_mssql::doctor::DataTables::new(),
    )
    .await
    .expect("read permissions")
}

/// The gaps as the report spells them, sorted so an assertion names the whole
/// set rather than whichever one came first.
fn named_gaps(held: &pbps_mssql::doctor::Held) -> Vec<String> {
    let mut out: Vec<String> = pbps_mssql::doctor::missing(held)
        .iter()
        .map(|g| format!("{} on {}", g.permission, g.securable()))
        .collect();
    out.sort();
    out
}

/// Reference data needs DML that `ALTER ON SCHEMA` does not confer, and it is
/// authorized on the **table** — both measured on the engine rather than
/// reasoned about.
///
/// This is the shape no unit test can settle. Three claims here are about SQL
/// Server: that an account granted everything `doctor` asked of a project with
/// no `data:` block still cannot write a declared row; that a grant on the
/// table alone satisfies the object question and not the schema one; and that
/// a `DENY` on the table refuses the statement while the schema grant still
/// answers 1. The only way to know any of them is to hold the grants and try
/// the statement.
///
/// The managed schema here is `app`, not `dbo`: the ledger's own `INSERT` and
/// `DELETE` on `dbo` would otherwise cover the very gap this is about.
#[tokio::test]
#[ignore = "needs a live SQL Server; set PBPS_TEST_DB (see scripts/live-tests.sh)"]
async fn declared_rows_need_dml_that_alter_on_the_schema_does_not_confer() {
    let mut db = TestDb::create("doctordata").await;
    let login = format!("pbps_dml_{}", std::process::id());
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
    // Exactly the list `doctor` printed before reference data was on it: the
    // managed permissions on `app`, the ledger's own on `dbo` (the tables do
    // not exist yet, so those fall back to the schema), and the four database
    // `CREATE`s. No DML anywhere near `app`.
    // `CREATE SCHEMA` has to be first in its batch, so it travels inside an
    // `EXEC`, which gives it one of its own.
    db.conn
        .execute(&format!(
            "USE [{0}]; \
             EXEC(N'CREATE SCHEMA app;'); \
             CREATE USER [{login}] FOR LOGIN [{login}]; \
             GRANT VIEW DEFINITION, SELECT, ALTER, REFERENCES ON SCHEMA::app TO [{login}]; \
             GRANT SELECT, INSERT, DELETE, ALTER ON SCHEMA::dbo TO [{login}]; \
             GRANT CREATE TABLE, CREATE VIEW, CREATE PROCEDURE, CREATE FUNCTION TO [{login}];",
            db.name
        ))
        .await
        .expect("grant");
    // The table the declaration would carry rows for, created by the owner so
    // that the login's failure below is about DML and not about the DDL.
    db.conn
        .execute(
            "CREATE TABLE app.t (code varchar(20) NOT NULL PRIMARY KEY, \
             label nvarchar(50) NOT NULL); INSERT INTO app.t VALUES ('a', N'A');",
        )
        .await
        .expect("create the data table");

    let base_no_credentials = conn_str()
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
        "{base_no_credentials};User Id={login};Password={password};Database={}",
        db.name
    );
    let mut lp = connect_live(&as_login).await.expect("connect as the login");

    // The premise, measured: `ALTER` on the schema lets this login change the
    // table's shape and still refuses every row statement on it. Three
    // separate refusals, because the three permissions are granted separately
    // and a single one would not say which of them `ALTER` covers.
    lp.execute("ALTER TABLE app.t ADD note nvarchar(10) NULL;")
        .await
        .expect("ALTER on the schema really does authorize the DDL");
    for statement in [
        "INSERT INTO app.t (code, label) VALUES ('b', N'B');",
        "UPDATE app.t SET label = N'B' WHERE code = 'a';",
        "DELETE FROM app.t WHERE code = 'a';",
    ] {
        let refused = lp.execute(statement).await;
        assert!(
            refused.is_err(),
            "ALTER on the schema must not confer DML, but `{statement}` succeeded"
        );
    }

    // With no `data:` block the account is ready, which is what makes every
    // assertion below about reference data and not about a grant this test
    // forgot.
    let none = pbps_mssql::doctor::DataTables::new();
    let held = data_permissions(&mut lp, &none).await;
    let gaps = pbps_mssql::doctor::missing(&held);
    assert!(
        gaps.is_empty(),
        "a project declaring no row was reported as missing: {gaps:?}"
    );

    // Declaring rows in `app.t`, the three refusals above become three gaps,
    // on the table — the report the operator needed before `apply` took the
    // lock.
    let exact: pbps_mssql::doctor::DataTables = [("app.t".parse().unwrap(), seeded_table())]
        .into_iter()
        .collect();
    let held = data_permissions(&mut lp, &exact).await;
    assert_eq!(
        named_gaps(&held),
        [
            "DELETE on OBJECT::[app].[t]",
            "INSERT on OBJECT::[app].[t]",
            "UPDATE on OBJECT::[app].[t]",
        ],
        "{held:?}"
    );

    // Granted on the **table** and nowhere wider — the shape a careful DBA
    // reaches for, and the one a schema-scoped question calls a gap. The
    // account is ready and the row statements run.
    db.conn
        .execute(&format!(
            "USE [{0}]; GRANT INSERT, UPDATE, DELETE ON app.t TO [{login}];",
            db.name
        ))
        .await
        .expect("grant the DML on the table");
    let mut lp = connect_live(&as_login).await.expect("reconnect");
    let held = data_permissions(&mut lp, &exact).await;
    assert!(
        !held.schemas["app"].contains("INSERT"),
        "the premise: nothing is held on the schema: {held:?}"
    );
    let gaps = pbps_mssql::doctor::missing(&held);
    assert!(gaps.is_empty(), "{gaps:?}");
    for statement in [
        "INSERT INTO app.t (code, label) VALUES ('b', N'B');",
        "UPDATE app.t SET label = N'B' WHERE code = 'a';",
        "DELETE FROM app.t WHERE code = 'a';",
    ] {
        lp.execute(statement)
            .await
            .unwrap_or_else(|e| panic!("`{statement}` was still refused: {e}"));
    }

    // The converse, and the worse direction: the whole schema granted with the
    // one table denied. A schema-scoped question answers 1 and would report
    // ready; the statement really fails. Only the object question sees it.
    db.conn
        .execute(&format!(
            "USE [{0}]; \
             REVOKE INSERT, UPDATE, DELETE ON app.t FROM [{login}]; \
             GRANT INSERT, UPDATE, DELETE ON SCHEMA::app TO [{login}]; \
             DENY INSERT ON app.t TO [{login}];",
            db.name
        ))
        .await
        .expect("grant the schema and deny the table");
    let mut lp = connect_live(&as_login).await.expect("reconnect");
    let held = data_permissions(&mut lp, &exact).await;
    assert!(
        held.schemas["app"].contains("INSERT"),
        "the premise: the schema grant stands: {held:?}"
    );
    assert!(
        lp.execute("INSERT INTO app.t (code, label) VALUES ('c', N'C');")
            .await
            .is_err(),
        "the premise: the DENY really refuses the statement"
    );
    assert_eq!(
        named_gaps(&held),
        ["INSERT on OBJECT::[app].[t]"],
        "{held:?}"
    );

    // A table this deployment has still to create: no object to ask about, so
    // the question falls back to its schema — every first deployment of a
    // project that seeds rows. `app` carries the DML here, so it is ready.
    let unbuilt: pbps_mssql::doctor::DataTables =
        [("app.unbuilt".parse().unwrap(), seeded_table())]
            .into_iter()
            .collect();
    let held = data_permissions(&mut lp, &unbuilt).await;
    assert!(
        held.data_objects.is_empty(),
        "a table that is not there has no object row: {held:?}"
    );
    let gaps = pbps_mssql::doctor::missing(&held);
    assert!(gaps.is_empty(), "the schema grant covers it: {gaps:?}");

    // And with the schema's DML gone, that same fallback is what reports the
    // gap — at the schema, the only place a grant can sit before the table
    // exists.
    db.conn
        .execute(&format!(
            "USE [{0}]; REVOKE INSERT, UPDATE, DELETE ON SCHEMA::app FROM [{login}];",
            db.name
        ))
        .await
        .expect("revoke the schema DML");
    let mut lp = connect_live(&as_login).await.expect("reconnect");
    let held = data_permissions(&mut lp, &unbuilt).await;
    assert_eq!(
        named_gaps(&held),
        [
            "DELETE on SCHEMA::[app]",
            "INSERT on SCHEMA::[app]",
            "UPDATE on SCHEMA::[app]",
        ],
        "{held:?}"
    );

    drop(lp);
    // Best-effort, like the other login tests here: the server may still count
    // a just-closed session as logged in, and a tidy-up that failed must not
    // be reported as this test failing. The container is throwaway.
    let _ = db
        .conn
        .execute(&format!(
            "USE master; IF SUSER_ID('{login}') IS NOT NULL DROP LOGIN [{login}];"
        ))
        .await;
    db.drop().await;
}

/// The base a least-privilege login needs before any of the questions this
/// file asks about reference data: the managed permissions on `app`, the
/// ledger's own on `dbo`, and the four database `CREATE`s. Everything the
/// tests below then add or take away is DML, so a gap they report is about
/// the grant under test and not about a permission the setup forgot.
///
/// `CREATE SCHEMA` has to be first in its batch, so it travels inside an
/// `EXEC`, which gives it one of its own.
async fn least_privilege_login(db: &mut TestDb, login: &str, password: &str) -> String {
    db.conn
        .execute(&format!(
            "USE master; \
             IF SUSER_ID('{login}') IS NOT NULL DROP LOGIN [{login}]; \
             CREATE LOGIN [{login}] WITH PASSWORD = '{password}', CHECK_POLICY = OFF;"
        ))
        .await
        .expect("create login");
    db.conn
        .execute(&format!(
            "USE [{0}]; \
             EXEC(N'CREATE SCHEMA app;'); \
             CREATE USER [{login}] FOR LOGIN [{login}]; \
             GRANT VIEW DEFINITION, SELECT, ALTER, REFERENCES ON SCHEMA::app TO [{login}]; \
             GRANT SELECT, INSERT, DELETE, ALTER ON SCHEMA::dbo TO [{login}]; \
             GRANT CREATE TABLE, CREATE VIEW, CREATE PROCEDURE, CREATE FUNCTION TO [{login}];",
            db.name
        ))
        .await
        .expect("grant the base");
    let base_no_credentials = conn_str()
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
    format!(
        "{base_no_credentials};User Id={login};Password={password};Database={}",
        db.name
    )
}

/// Whether this connection's account holds a permission on an object at
/// **object** scope, asked the plain way — the premise every assertion below
/// rests on, read from the engine rather than assumed.
async fn holds_at_object_scope(conn: &mut Conn, object: &str, permission: &str) -> bool {
    let rows = conn
        .query(&format!(
            "SELECT HAS_PERMS_BY_NAME('{object}', 'OBJECT', '{permission}') AS held;"
        ))
        .await
        .expect("ask the object question");
    let row = rows.first().expect("one row");
    let held: i32 = row.try_get("held").expect("held").unwrap_or(-1);
    held == 1
}

/// A column-level `GRANT` is a grant, and the readiness check has to see it.
///
/// SQL Server authorizes `UPDATE` column by column. An account granted exactly
/// the columns a declaration writes holds nothing at object scope — the object
/// question answers 0 — and its `UPDATE` runs. `doctor` used to report that as
/// a gap it did not have, and the remedy it printed (`GRANT UPDATE ON app.t`)
/// widened a deliberately narrow grant.
///
/// Nothing here can be settled without the engine. That the object question
/// answers 0 under a column grant, that the column question answers 1, that
/// the statement really runs, and — the case that decides which column list to
/// ask about — that a column added *after* a column-level grant is not covered
/// by it while one added after an object-level grant is.
#[tokio::test]
#[ignore = "needs a live SQL Server; set PBPS_TEST_DB (see scripts/live-tests.sh)"]
async fn a_column_level_grant_is_seen_where_the_object_level_answer_is_zero() {
    let mut db = TestDb::create("doctorcol").await;
    let login = format!("pbps_col_{}", std::process::id());
    // Not a secret: this login exists for the length of one test inside a
    // throwaway container, and it is dropped below.
    let password = "pbpsLeastPrivilege!1";
    let as_login = least_privilege_login(&mut db, &login, password).await;

    // Created by the owner, so that the login's refusals below are about DML
    // and not about the DDL.
    db.conn
        .execute(
            "CREATE TABLE app.t (code varchar(20) NOT NULL PRIMARY KEY, \
             label nvarchar(50) NULL, note nvarchar(50) NULL); \
             INSERT INTO app.t VALUES ('a', N'A', N'N');",
        )
        .await
        .expect("create the data table");
    // `INSERT` and `DELETE` on the object, so `UPDATE` is the only permission
    // in question — those two SQL Server does not take at column scope at all.
    // Then `UPDATE` on exactly the two columns the declaration can correct.
    db.conn
        .execute(&format!(
            "USE [{0}]; \
             GRANT INSERT, DELETE ON app.t TO [{login}]; \
             GRANT UPDATE ON app.t(label) TO [{login}]; \
             GRANT UPDATE ON app.t(note) TO [{login}];",
            db.name
        ))
        .await
        .expect("grant the column-level UPDATE");

    let declared: pbps_mssql::doctor::DataTables = [(
        "app.t".parse().unwrap(),
        seeded_table_with(&["label", "note"]),
    )]
    .into_iter()
    .collect();
    let mut lp = connect_live(&as_login).await.expect("connect as the login");
    // The premise: nothing at object scope, and the statement runs anyway.
    assert!(
        !holds_at_object_scope(&mut lp, "app.t", "UPDATE").await,
        "the premise: a column-level grant answers 0 at object scope"
    );
    lp.execute("UPDATE app.t SET label = N'B', note = N'M' WHERE code = 'a';")
        .await
        .expect("the premise: the column grant really authorizes the UPDATE");
    let held = data_permissions(&mut lp, &declared).await;
    assert!(
        held.data_objects[&"app.t".parse::<pbps_model::ObjectName>().unwrap()].contains("UPDATE"),
        "the column-level grant is what the object question answers with: {held:?}"
    );
    let gaps = pbps_mssql::doctor::missing(&held);
    assert!(gaps.is_empty(), "{gaps:?}");

    // One writable column short of the declaration, and it is a gap again —
    // reported on the object, which is where the operator's `GRANT` goes.
    // `doctor` never sees a plan, so "held on some column" is not a question
    // it could act on: the `UPDATE` may name any of them.
    db.conn
        .execute(&format!(
            "USE [{0}]; REVOKE UPDATE ON app.t(note) FROM [{login}];",
            db.name
        ))
        .await
        .expect("revoke one column");
    let mut lp = connect_live(&as_login).await.expect("reconnect");
    assert!(
        lp.execute("UPDATE app.t SET note = N'X' WHERE code = 'a';")
            .await
            .is_err(),
        "the premise: the revoked column really refuses the statement"
    );
    let held = data_permissions(&mut lp, &declared).await;
    assert_eq!(
        named_gaps(&held),
        ["UPDATE on OBJECT::[app].[t]"],
        "{held:?}"
    );

    // The column a plan is about to add. It is not in the catalog, so a
    // column list taken from there would say "every column is granted" and
    // call this ready for an `UPDATE` that will name a column the account
    // holds nothing on. Asked as *declared*, it answers 0.
    db.conn
        .execute(&format!(
            "USE [{0}]; GRANT UPDATE ON app.t(note) TO [{login}];",
            db.name
        ))
        .await
        .expect("restore the second column");
    let adds_a_column: pbps_mssql::doctor::DataTables = [(
        "app.t".parse().unwrap(),
        seeded_table_with(&["label", "note", "extra"]),
    )]
    .into_iter()
    .collect();
    let mut lp = connect_live(&as_login).await.expect("reconnect");
    assert!(
        pbps_mssql::doctor::missing(&data_permissions(&mut lp, &declared).await).is_empty(),
        "the premise: the two existing columns are granted again"
    );
    assert_eq!(
        named_gaps(&data_permissions(&mut lp, &adds_a_column).await),
        ["UPDATE on OBJECT::[app].[t]"],
        "a declared column that is not there yet is not granted either"
    );

    // And that gap is the truth, not caution: the column-level grants do not
    // reach a column added after them, and the object-level grant does.
    db.conn
        .execute("ALTER TABLE app.t ADD extra nvarchar(50) NULL;")
        .await
        .expect("add the column the declaration names");
    let mut lp = connect_live(&as_login).await.expect("reconnect");
    assert!(
        lp.execute("UPDATE app.t SET extra = N'X' WHERE code = 'a';")
            .await
            .is_err(),
        "a column-level grantee is refused the column added after its grants"
    );
    assert_eq!(
        named_gaps(&data_permissions(&mut lp, &adds_a_column).await),
        ["UPDATE on OBJECT::[app].[t]"],
        "the same gap, now that the column exists and is ungranted"
    );
    db.conn
        .execute(&format!(
            "USE [{0}]; GRANT UPDATE ON app.t TO [{login}];",
            db.name
        ))
        .await
        .expect("grant UPDATE on the object");
    let mut lp = connect_live(&as_login).await.expect("reconnect");
    lp.execute("UPDATE app.t SET extra = N'X' WHERE code = 'a';")
        .await
        .expect("an object-level grantee reaches a column added after the grant");
    let gaps = pbps_mssql::doctor::missing(&data_permissions(&mut lp, &adds_a_column).await);
    assert!(gaps.is_empty(), "{gaps:?}");

    drop(lp);
    // Best-effort, like the other login tests here: the server may still count
    // the session, and a login this container will drop with it must not be
    // reported as this test failing.
    let _ = db
        .conn
        .execute(&format!(
            "USE master; IF SUSER_ID('{login}') IS NOT NULL DROP LOGIN [{login}];"
        ))
        .await;
    db.drop().await;
}

/// The same grant, on a securable whose columns pbps does not declare: a
/// foreign-key target outside the managed schemas.
///
/// There the column list is the catalog's own, which is the whole set a
/// statement could name — nothing this tool does adds a column to someone
/// else's table. `SELECT` and `REFERENCES` are both taken at column scope, so
/// an account granted them column by column on the parent is ready, and one
/// column short of it is not.
#[tokio::test]
#[ignore = "needs a live SQL Server; set PBPS_TEST_DB (see scripts/live-tests.sh)"]
async fn a_column_level_grant_on_a_referenced_table_is_read_from_the_catalog() {
    let mut db = TestDb::create("doctorrefcol").await;
    let login = format!("pbps_refcol_{}", std::process::id());
    let password = "pbpsLeastPrivilege!1";
    let as_login = least_privilege_login(&mut db, &login, password).await;
    db.conn
        .execute(
            "EXEC(N'CREATE SCHEMA shared;'); \
             CREATE TABLE shared.parent (code varchar(20) NOT NULL PRIMARY KEY, \
             label nvarchar(50) NULL);",
        )
        .await
        .expect("create the referenced table");

    let referenced: Vec<pbps_model::ObjectName> = vec!["shared.parent".parse().unwrap()];

    // Every column of the parent, and nothing at object scope.
    db.conn
        .execute(&format!(
            "USE [{0}]; \
             GRANT SELECT, REFERENCES ON shared.parent(code) TO [{login}]; \
             GRANT SELECT, REFERENCES ON shared.parent(label) TO [{login}];",
            db.name
        ))
        .await
        .expect("grant column by column");
    let mut lp = connect_live(&as_login).await.expect("connect as the login");
    assert!(
        !holds_at_object_scope(&mut lp, "shared.parent", "SELECT").await,
        "the premise: a column-level grant answers 0 at object scope"
    );
    lp.execute("SELECT 1 FROM shared.parent;")
        .await
        .expect("the premise: the probe this permission is for really runs");
    let gaps = pbps_mssql::doctor::missing(&referenced_permissions(&mut lp, &referenced).await);
    assert!(gaps.is_empty(), "{gaps:?}");

    // One column short: the probe reads the whole row, so a column it cannot
    // read is a gap — and it is reported on the object, where the `GRANT`
    // goes.
    db.conn
        .execute(&format!(
            "USE [{0}]; REVOKE SELECT ON shared.parent(label) FROM [{login}];",
            db.name
        ))
        .await
        .expect("revoke one column");
    let mut lp = connect_live(&as_login).await.expect("reconnect");
    let held = referenced_permissions(&mut lp, &referenced).await;
    assert_eq!(
        named_gaps(&held),
        ["SELECT on OBJECT::[shared].[parent]"],
        "{held:?}"
    );

    drop(lp);
    let _ = db
        .conn
        .execute(&format!(
            "USE master; IF SUSER_ID('{login}') IS NOT NULL DROP LOGIN [{login}];"
        ))
        .await;
    db.drop().await;
}

/// `DENY` at a narrower securable beats an inherited `CONTROL`, and the
/// readiness check has to see it.
///
/// `missing` used to return early on `CONTROL` at the database. The reasoning
/// was that `CONTROL` implies everything below it — true of grants, and beside
/// the point, because the inputs are `HAS_PERMS_BY_NAME` answers that already
/// account for inheritance. So the shortcut bought nothing for an owner and
/// discarded the only answer that matters here.
///
/// Only a real server can settle it: whether `fn_my_permissions` still lists
/// `CONTROL` under a `DENY`, whether the scoped question answers 0, and whether
/// the DDL actually fails are three separate facts, and the shortcut was built
/// on the first one alone.
#[tokio::test]
#[ignore = "needs a live SQL Server; set PBPS_TEST_DB (see scripts/live-tests.sh)"]
async fn a_deny_beats_control_and_the_readiness_check_sees_it() {
    let mut db = TestDb::create("doctordeny").await;
    let login = format!("pbps_ctl_{}", std::process::id());
    // Not a secret: this login exists for the length of one test inside a
    // throwaway container, and it is dropped below.
    let password = "pbpsControl!1";

    db.conn
        .execute(&format!(
            "USE master;              IF SUSER_ID('{login}') IS NOT NULL DROP LOGIN [{login}];              CREATE LOGIN [{login}] WITH PASSWORD = '{password}', CHECK_POLICY = OFF;"
        ))
        .await
        .expect("create login");
    db.conn
        .execute(&format!("USE [{}];", db.name))
        .await
        .expect("use");
    db.conn
        .execute("CREATE SCHEMA [app];")
        .await
        .expect("create app schema");
    db.conn
        .execute(&format!(
            "CREATE USER [{login}] FOR LOGIN [{login}]; GRANT CONTROL TO [{login}];"
        ))
        .await
        .expect("grant control");

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

    // The premise: an owner is clean, and is clean *without* the shortcut —
    // its scoped answers come back full on their own.
    let mut lp = connect_live(&as_login).await.expect("connect as the login");
    let held = pbps_mssql::doctor::permissions(
        &mut lp,
        &["app".to_owned()],
        &[],
        &pbps_mssql::doctor::GrantTargets::default(),
        &pbps_mssql::doctor::DataTables::new(),
    )
    .await
    .expect("read permissions");
    assert!(
        held.database.contains("CONTROL"),
        "the test's premise is wrong if this login lacks CONTROL: {held:?}"
    );
    assert!(
        pbps_mssql::doctor::missing(&held).is_empty(),
        "an owner must not be reported as missing anything: {:?}",
        pbps_mssql::doctor::missing(&held)
    );

    db.conn
        .execute(&format!("DENY ALTER ON SCHEMA::app TO [{login}];"))
        .await
        .expect("deny");
    let mut lp = connect_live(&as_login).await.expect("reconnect");
    let held = pbps_mssql::doctor::permissions(
        &mut lp,
        &["app".to_owned()],
        &[],
        &pbps_mssql::doctor::GrantTargets::default(),
        &pbps_mssql::doctor::DataTables::new(),
    )
    .await
    .expect("read permissions");
    // All three facts, because the old shortcut was built on the first alone.
    assert!(
        held.database.contains("CONTROL"),
        "CONTROL is still listed under a DENY, which is the trap: {held:?}"
    );
    assert!(
        !held.schemas["app"].contains("ALTER"),
        "the scoped question has to see the DENY: {held:?}"
    );
    let denied = lp.execute("CREATE TABLE app.denied_probe (id int);").await;
    assert!(
        denied.is_err(),
        "the premise is wrong if the DDL succeeds under the DENY"
    );

    let gaps = pbps_mssql::doctor::missing(&held);
    assert!(
        gaps.iter()
            .any(|g| g.permission == "ALTER" && g.securable() == "SCHEMA::[app]"),
        "an account that cannot alter its managed schema must not pass readiness: {gaps:?}"
    );

    drop(lp);
    db.drop().await;
    let mut admin = connect_live(&conn_str()).await.expect("connect");
    let _ = admin
        .execute(&format!(
            "USE master; IF SUSER_ID('{login}') IS NOT NULL DROP LOGIN [{login}];"
        ))
        .await;
}

/// A lock table the caller may not read is not an absent one.
///
/// SQL Server's metadata-visibility rules hide an object from a principal with
/// no permission on it, so the `OBJECT_ID` guard this replaced answered "absent"
/// for a table that exists and holds a live lock — turning "not authorized to
/// look" into "no lock", which is the one direction this tool must never round
/// in. `HAS_PERMS_BY_NAME` does not separate them either: it answers 0 for both.
///
/// Only a real server settles which of those three questions can tell the cases
/// apart, which is why this test exists at all rather than a unit one.
#[tokio::test]
#[ignore = "needs a live SQL Server; set PBPS_TEST_DB (see scripts/live-tests.sh)"]
async fn a_lock_table_that_cannot_be_read_is_not_an_absent_one() {
    let mut db = TestDb::create("lockvisibility").await;
    let login = format!("pbps_viz_{}", std::process::id());
    // Not a secret: this login exists for the length of one test inside a
    // throwaway container, and it is dropped below.
    let password = "pbpsVisibility!1";

    db.conn
        .execute(&format!(
            "USE master; \
             IF SUSER_ID('{login}') IS NOT NULL DROP LOGIN [{login}]; \
             CREATE LOGIN [{login}] WITH PASSWORD = '{password}', CHECK_POLICY = OFF;"
        ))
        .await
        .expect("create login");
    db.conn
        .execute(&format!("USE [{}];", db.name))
        .await
        .expect("use");
    pbps_mssql::state::ensure_tables(&mut db.conn)
        .await
        .expect("create the ledger");
    pbps_mssql::state::lock(&mut db.conn, "the-invisible-holder")
        .await
        .expect("take the lock");
    // The state table is readable, the lock table is not. That is the shape a
    // half-granted deployment account really has, and `doctor` exists to catch
    // it — but only if the lock read reports rather than shrugs.
    db.conn
        .execute(&format!(
            "CREATE USER [{login}] FOR LOGIN [{login}]; \
             GRANT SELECT ON dbo.__pbps_state TO [{login}];"
        ))
        .await
        .expect("grant on the state table only");

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

    let mut lp = connect_live(&as_login).await.expect("connect as the login");
    // The premise, stated rather than assumed: the catalog really does hide the
    // table from this principal, so the old guard really would have said absent.
    let hidden = lp
        .query(
            "SELECT CASE WHEN OBJECT_ID(N'dbo.__pbps_lock', N'U') IS NULL THEN 0 ELSE 1 END \
             AS present;",
        )
        .await
        .expect("ask the catalog");
    let present: i32 = hidden[0].try_get("present").expect("present").unwrap_or(1);
    assert_eq!(present, 0, "the premise is wrong if the table is visible");

    let answer = pbps_mssql::state::lock_holder(&mut lp).await;
    assert!(
        answer.is_err(),
        "a lock that cannot be read must not be reported as absent: {answer:?}"
    );

    // And the other half, on the same connection: a table that genuinely is not
    // there still answers "no lock" rather than failing, which is what keeps a
    // first run quiet.
    db.conn
        .execute("DROP TABLE dbo.__pbps_lock;")
        .await
        .expect("drop the lock table");
    let mut lp = connect_live(&as_login).await.expect("reconnect");
    assert!(
        pbps_mssql::state::lock_holder(&mut lp)
            .await
            .expect("an absent lock table is not an error")
            .is_none()
    );

    drop(lp);
    db.drop().await;
    let mut admin = connect_live(&conn_str()).await.expect("connect");
    let _ = admin
        .execute(&format!(
            "USE master; IF SUSER_ID('{login}') IS NOT NULL DROP LOGIN [{login}];"
        ))
        .await;
}

/// The ledger table has the same metadata-visibility fault the lock table had.
///
/// `is_initialized` asked `OBJECT_ID(N'dbo.__pbps_state', N'U') IS NULL`, and a
/// principal with no permission on an existing ledger gets NULL from it — the
/// answer an absent table gives. Every caller then reported the environment as
/// never initialized: `doctor` said `uninitialized`, `explain` offered
/// `bootstrap`, and `state list` printed an empty history for a database with
/// years of it. Measured on this server, `HAS_PERMS_BY_NAME` answers 0 for both
/// cases as well; only attempting the statement separates them, 208 from 229.
///
/// Live for the same reason as its sibling above: what the catalog hides and
/// what the statement returns is a question only a real server answers.
#[tokio::test]
#[ignore = "needs a live SQL Server; set PBPS_TEST_DB (see scripts/live-tests.sh)"]
async fn a_ledger_that_cannot_be_read_is_not_an_uninitialized_one() {
    let mut db = TestDb::create("statevisibility").await;
    let login = format!("pbps_sviz_{}", std::process::id());
    // Not a secret: this login exists for the length of one test inside a
    // throwaway container, and it is dropped below.
    let password = "pbpsStateVisibility!1";

    db.conn
        .execute(&format!(
            "USE master; \
             IF SUSER_ID('{login}') IS NOT NULL DROP LOGIN [{login}]; \
             CREATE LOGIN [{login}] WITH PASSWORD = '{password}', CHECK_POLICY = OFF;"
        ))
        .await
        .expect("create login");
    db.conn
        .execute(&format!("USE [{}];", db.name))
        .await
        .expect("use");
    pbps_mssql::state::ensure_tables(&mut db.conn)
        .await
        .expect("create the ledger");
    // The user exists and may connect; nothing is granted on `__pbps_state`.
    db.conn
        .execute(&format!("CREATE USER [{login}] FOR LOGIN [{login}];"))
        .await
        .expect("create the user");

    let base = conn_str();
    let base_no_credentials = base
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
        "{base_no_credentials};User Id={login};Password={password};Database={}",
        db.name
    );

    let mut lp = connect_live(&as_login).await.expect("connect as the login");
    // The premise, stated rather than assumed: the catalog really does hide the
    // ledger from this principal, so the old guard really would have said absent.
    let hidden = lp
        .query(
            "SELECT CASE WHEN OBJECT_ID(N'dbo.__pbps_state', N'U') IS NULL THEN 0 ELSE 1 END \
             AS present;",
        )
        .await
        .expect("ask the catalog");
    let present: i32 = hidden[0].try_get("present").expect("present").unwrap_or(1);
    assert_eq!(present, 0, "the premise is wrong if the table is visible");

    let answer = pbps_mssql::state::is_initialized(&mut lp).await;
    assert!(
        answer.is_err(),
        "a ledger that cannot be read must not be reported as absent: {answer:?}"
    );
    // And the readers built on it report rather than shrug.
    assert!(
        matches!(
            pbps_mssql::state::timeline(&mut lp, 10).await,
            Err(pbps_db::LedgerError::Db(_))
        ),
        "the timeline of an unreadable ledger is an error, not an empty list"
    );

    // The other half, on a database that genuinely has no ledger: still
    // `false`, which is what keeps a first run quiet.
    let mut fresh = TestDb::create("statevisibilitynone").await;
    fresh
        .conn
        .execute(&format!(
            "CREATE USER [{login}] FOR LOGIN [{login}]; GRANT CONTROL TO [{login}];"
        ))
        .await
        .expect("create the user");
    let as_login_fresh = format!(
        "{base_no_credentials};User Id={login};Password={password};Database={}",
        fresh.name
    );
    let mut none = connect_live(&as_login_fresh).await.expect("connect");
    assert!(
        !pbps_mssql::state::is_initialized(&mut none)
            .await
            .expect("an absent ledger is not an error"),
        "an absent ledger is absent"
    );

    drop(lp);
    drop(none);
    fresh.drop().await;
    db.drop().await;
    let mut admin = connect_live(&conn_str()).await.expect("connect");
    let _ = admin
        .execute(&format!(
            "USE master; IF SUSER_ID('{login}') IS NOT NULL DROP LOGIN [{login}];"
        ))
        .await;
}

/// A foreign key into a schema the project does not manage.
///
/// `validate` accepts one whose target is undeclared — the target is somebody
/// else's table — but the emitter still writes `REFERENCES [shared].[parent]`,
/// which SQL Server authorizes on *that* table, and the pre-flight probe for
/// the change reads it. Neither is covered by any question about the managed
/// schemas, so a login could pass `doctor` and fail during `apply`.
///
/// Live because the whole question is what a real server authorizes: only it
/// can say that `ALTER` on `app` does not carry `REFERENCES` on `shared.parent`.
#[tokio::test]
#[ignore = "needs a live SQL Server; set PBPS_TEST_DB (see scripts/live-tests.sh)"]
async fn a_foreign_key_into_an_unmanaged_schema_needs_permission_on_its_target() {
    let mut db = TestDb::create("fkacross").await;
    let login = format!("pbps_fk_{}", std::process::id());
    // Not a secret: this login exists for the length of one test inside a
    // throwaway container, and it is dropped below.
    let password = "pbpsForeignKey!1";

    db.conn
        .execute(&format!(
            "USE master; \
             IF SUSER_ID('{login}') IS NOT NULL DROP LOGIN [{login}]; \
             CREATE LOGIN [{login}] WITH PASSWORD = '{password}', CHECK_POLICY = OFF;"
        ))
        .await
        .expect("create login");
    db.conn
        .execute(&format!("USE [{}];", db.name))
        .await
        .expect("use");
    // `CREATE SCHEMA` must be the first statement in its batch, so one each.
    db.conn.execute("CREATE SCHEMA [app];").await.expect("app");
    db.conn
        .execute("CREATE SCHEMA [shared];")
        .await
        .expect("shared");
    db.conn
        .execute(
            "CREATE TABLE shared.parent (id bigint NOT NULL CONSTRAINT pk_parent PRIMARY KEY);",
        )
        .await
        .expect("the unmanaged parent table");
    // Everything a project managing `app` needs, and nothing at all on
    // `shared` — the shape a DBA produces when told "grant them their schema".
    db.conn
        .execute(&format!(
            "CREATE USER [{login}] FOR LOGIN [{login}]; \
             GRANT VIEW DEFINITION, SELECT, INSERT, DELETE, ALTER, REFERENCES \
             ON SCHEMA::app TO [{login}]; \
             GRANT VIEW DEFINITION, SELECT, INSERT, DELETE, ALTER, REFERENCES \
             ON SCHEMA::dbo TO [{login}]; \
             GRANT CREATE TABLE, CREATE VIEW, CREATE PROCEDURE, CREATE FUNCTION TO [{login}];"
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

    let mut lp = connect_live(&as_login).await.expect("connect as the login");
    // The premise: with no foreign key out of `app`, this login is ready. If it
    // were not, the assertion below would pass for the wrong reason.
    let held = pbps_mssql::doctor::permissions(
        &mut lp,
        &["app".to_owned()],
        &[],
        &pbps_mssql::doctor::GrantTargets::default(),
        &pbps_mssql::doctor::DataTables::new(),
    )
    .await
    .expect("read permissions");
    assert!(
        pbps_mssql::doctor::missing(&held).is_empty(),
        "the premise is wrong if this login is short of something else: {:?}",
        pbps_mssql::doctor::missing(&held)
    );

    // And with one, the target it points at is asked about.
    let held = pbps_mssql::doctor::permissions(
        &mut lp,
        &["app".to_owned()],
        &["shared.parent".parse().unwrap()],
        &pbps_mssql::doctor::GrantTargets::default(),
        &pbps_mssql::doctor::DataTables::new(),
    )
    .await
    .expect("read permissions");
    let gaps = pbps_mssql::doctor::missing(&held);
    for permission in ["REFERENCES", "SELECT"] {
        assert!(
            gaps.iter()
                .any(|g| g.permission == permission && g.securable() == "OBJECT::[shared].[parent]"),
            "{permission} on the referenced table was not reported: {gaps:?}"
        );
    }

    // Granted on the object alone — the narrowest thing a DBA can do — and the
    // report goes quiet. A schema-scoped question could not see this grant.
    db.conn
        .execute(&format!(
            "GRANT REFERENCES, SELECT ON OBJECT::shared.parent TO [{login}];"
        ))
        .await
        .expect("grant on the object");
    let mut lp = connect_live(&as_login).await.expect("reconnect");
    let held = pbps_mssql::doctor::permissions(
        &mut lp,
        &["app".to_owned()],
        &["shared.parent".parse().unwrap()],
        &pbps_mssql::doctor::GrantTargets::default(),
        &pbps_mssql::doctor::DataTables::new(),
    )
    .await
    .expect("read permissions");
    assert!(
        pbps_mssql::doctor::missing(&held).is_empty(),
        "an object-scoped grant on the target must satisfy it: {:?}",
        pbps_mssql::doctor::missing(&held)
    );

    drop(lp);
    db.drop().await;
    let mut admin = connect_live(&conn_str()).await.expect("connect");
    let _ = admin
        .execute(&format!(
            "USE master; IF SUSER_ID('{login}') IS NOT NULL DROP LOGIN [{login}];"
        ))
        .await;
}

/// ADR-0004 against the engine: the DML the emitter writes, in the order the
/// differ sorts it, on a real table. Three things only a live server can say:
/// that `SET IDENTITY_INSERT` around an insert pins an `IDENTITY` key and
/// leaves the switch off for the next table; that `SET ... = DEFAULT` makes
/// the engine evaluate the column's default; and that a parent row deleted
/// *after* the child row moved away from it is an order the foreign key
/// accepts — the shape that, the other way round, `ON DELETE CASCADE` would
/// have swallowed silently.
#[tokio::test]
#[ignore = "needs a live SQL Server; run scripts/live-tests.sh"]
async fn reference_data_reaches_the_engine_in_an_order_it_accepts() {
    use pbps_model::{DataMode, Row, RowKey, TableData, Value};

    fn rows(rows: &[(&str, &[(&str, Value)])]) -> std::collections::BTreeMap<RowKey, Row> {
        rows.iter()
            .map(|(k, cells)| {
                (
                    RowKey::from(*k),
                    cells
                        .iter()
                        .map(|(c, v)| ((*c).to_owned(), v.clone()))
                        .collect::<Row>(),
                )
            })
            .collect()
    }
    let text = |s: &str| Value::Text(s.to_owned());

    // `status`: a varchar key, and a NOT NULL label with a default.
    let mut status = Table::default();
    status
        .columns
        .insert("code".to_owned(), Column::new(ty("varchar(20)")).not_null());
    let mut label = Column::new(ty("nvarchar(50)")).not_null();
    label.default = Some("'Unlabelled'".to_owned());
    status.columns.insert("label".to_owned(), label);
    status.primary_key = Some(PrimaryKey {
        name: None,
        columns: vec!["code".to_owned()],
    });
    status.data = Some(TableData {
        mode: DataMode::Exact,
        rows: rows(&[
            ("new", &[("label", text("New"))]),
            ("old", &[("label", text("Old"))]),
        ]),
    });

    // `kind`: an IDENTITY key, and a foreign key to `status`.
    let mut kind = Table::default();
    let mut id = Column::new(ty("int")).not_null();
    id.identity = Some(pbps_model::Identity {
        seed: 1,
        increment: 1,
    });
    kind.columns.insert("id".to_owned(), id);
    kind.columns.insert(
        "status_code".to_owned(),
        Column::new(ty("varchar(20)")).not_null(),
    );
    kind.primary_key = Some(PrimaryKey {
        name: None,
        columns: vec!["id".to_owned()],
    });
    kind.foreign_keys.insert(
        "fk_kind_status".to_owned(),
        pbps_model::ForeignKey {
            columns: vec!["status_code".to_owned()],
            references_table: TableName::new("dbo", "status"),
            references_columns: vec!["code".to_owned()],
            on_delete: Default::default(),
            on_update: Default::default(),
        },
    );
    // Key 7, not 1: the first identity value the engine would hand out is 1,
    // so a pinned 1 proves nothing.
    kind.data = Some(TableData {
        mode: DataMode::Exact,
        rows: rows(&[("7", &[("status_code", text("old"))])]),
    });

    let mut declared = Schema::default();
    declared
        .tables
        .insert(TableName::new("dbo", "status"), status);
    declared.tables.insert(TableName::new("dbo", "kind"), kind);
    let ids = mint_ids(&declared, &IdsFile::default(), &[]);

    let mut db = TestDb::create("refdata").await;
    apply(
        &mut db.conn,
        &plan(&Schema::default(), &IdsFile::default(), &declared, &ids),
    )
    .await;

    async fn count(conn: &mut Conn, sql: &str) -> i32 {
        let rows = conn
            .query(sql)
            .await
            .unwrap_or_else(|e| panic!("the engine rejected:\n{sql}\n{e}"));
        rows[0].try_get_at(0).unwrap().unwrap()
    }

    assert_eq!(
        count(&mut db.conn, "SELECT COUNT(*) FROM dbo.status;").await,
        2
    );
    assert_eq!(
        count(
            &mut db.conn,
            "SELECT COUNT(*) FROM dbo.kind WHERE id = 7 AND status_code = 'old';"
        )
        .await,
        1,
        "the IDENTITY key must be pinned to the declared value"
    );

    // The second revision: `old` leaves `status`, `new` stops writing its
    // label (so it takes the default), kind 7 moves to `new`, and kind 9
    // arrives. The delete of `old` is only acceptable after kind 7 has moved.
    let mut next = declared.clone();
    next.tables
        .get_mut(&TableName::new("dbo", "status"))
        .unwrap()
        .data = Some(TableData {
        mode: DataMode::Exact,
        rows: rows(&[("new", &[])]),
    });
    next.tables
        .get_mut(&TableName::new("dbo", "kind"))
        .unwrap()
        .data = Some(TableData {
        mode: DataMode::Exact,
        rows: rows(&[
            ("7", &[("status_code", text("new"))]),
            ("9", &[("status_code", text("new"))]),
        ]),
    });
    let next_ids = mint_ids(&next, &ids, &[]);
    let second = plan(&declared, &ids, &next, &next_ids);

    // The plan holds each row to what it recorded (DECISIONS 122): `new`
    // was `New` when the plan was made, and a hand edit in between — the
    // checksum pins the state only up to the moment `apply` reads it — is
    // met by the statement, which names the row and rolls the plan back.
    // A change of case alone is a change, as it is to the drift check.
    for edited in ["Fresh", "NEW"] {
        db.conn
            .execute(&format!(
                "UPDATE dbo.status SET label = N'{edited}' WHERE code = 'new';"
            ))
            .await
            .expect("a hand edit after the plan");
        let err = try_apply(&mut db.conn, &second)
            .await
            .expect_err("the row is not as the plan recorded it");
        assert!(
            err.contains("dbo.status row `new` is not as the plan recorded it"),
            "{err}"
        );
        assert_eq!(
            count(
                &mut db.conn,
                "SELECT COUNT(*) FROM dbo.status WHERE code = 'old';"
            )
            .await,
            1,
            "nothing of the plan stays after the refusal"
        );
    }
    // A row deleted in between, the same way: a `DELETE` is held to the
    // row's existence.
    db.conn
        .execute("UPDATE dbo.status SET label = N'New' WHERE code = 'new';")
        .await
        .expect("the edit undone");
    let ghost = pbps_model::ChangeSet {
        changes: vec![pbps_model::PlannedChange::new(
            pbps_model::Change::DeleteRow {
                table: TableName::new("dbo", "status"),
                key_column: "code".to_owned(),
                key: RowKey::from("ghost"),
                cause: pbps_model::change::DeleteCause::Undeclared,
                row: std::collections::BTreeMap::new(),
                types: std::collections::BTreeMap::new(),
                after_types: Default::default(),
            },
        )],
    };
    let err = try_apply(&mut db.conn, &ghost)
        .await
        .expect_err("a row already gone is a baseline the plan was not reviewed against");
    assert!(
        err.contains("dbo.status row `ghost` is not as the plan recorded it"),
        "{err}"
    );
    // As recorded, the plan goes through.
    try_apply(&mut db.conn, &second)
        .await
        .expect("the rows are as the plan recorded them");

    assert_eq!(
        count(
            &mut db.conn,
            "SELECT COUNT(*) FROM dbo.status WHERE code = 'new' AND label = N'Unlabelled';"
        )
        .await,
        1,
        "`= DEFAULT` must make the engine evaluate the column's default"
    );
    assert_eq!(
        count(&mut db.conn, "SELECT COUNT(*) FROM dbo.status;").await,
        1,
        "`old` must be gone"
    );
    assert_eq!(
        count(
            &mut db.conn,
            "SELECT COUNT(*) FROM dbo.kind WHERE status_code = 'new' AND id IN (7, 9);"
        )
        .await,
        2
    );
    // And the switch is off again: an ordinary insert must let the engine
    // assign the key, which it cannot while IDENTITY_INSERT is on.
    db.conn
        .execute("INSERT INTO dbo.kind (status_code) VALUES ('new');")
        .await
        .expect("IDENTITY_INSERT must be off after the plan");

    db.drop().await;
}

/// The connected half of ADR-0004 against the engine: the rows a plan wrote
/// come back in the canonical form the declaration used, a hand edit and a
/// rogue row are seen, an `ensure` read stays inside its keys, and the
/// pre-delete probe counts what the catalog says still points at the row —
/// through dynamic SQL only a real server can validate.
#[tokio::test]
#[ignore = "needs a live SQL Server; run scripts/live-tests.sh"]
async fn declared_rows_read_back_as_declared_and_hand_edits_are_seen() {
    use pbps_model::{DataMode, Row, RowKey, RowScope, TableData, Value};

    fn rows(rows: &[(&str, &[(&str, Value)])]) -> std::collections::BTreeMap<RowKey, Row> {
        rows.iter()
            .map(|(k, cells)| {
                (
                    RowKey::from(*k),
                    cells
                        .iter()
                        .map(|(c, v)| ((*c).to_owned(), v.clone()))
                        .collect::<Row>(),
                )
            })
            .collect()
    }
    let text = |s: &str| Value::Text(s.to_owned());

    // `status`: a varchar key, a NOT NULL label with a default, an int that
    // may be NULL, and a `decimal` the declaration has to spell as the engine
    // does.
    let mut status = Table::default();
    status
        .columns
        .insert("code".to_owned(), Column::new(ty("varchar(20)")).not_null());
    let mut label = Column::new(ty("nvarchar(50)")).not_null();
    label.default = Some("'Unlabelled'".to_owned());
    status.columns.insert("label".to_owned(), label);
    status
        .columns
        .insert("rank".to_owned(), Column::new(ty("int")));
    status
        .columns
        .insert("pct".to_owned(), Column::new(ty("decimal(5,2)")));
    // Named, so the whole table compares equal below: the engine invents a
    // name for an unnamed key and the read-back reports it.
    status.primary_key = Some(PrimaryKey {
        name: Some("pk_status".to_owned()),
        columns: vec!["code".to_owned()],
    });
    status.data = Some(TableData {
        mode: DataMode::Exact,
        rows: rows(&[
            (
                "new",
                &[
                    ("label", text("New")),
                    ("rank", Value::Int(1)),
                    ("pct", text("1.50")),
                ],
            ),
            // Everything defaulted or NULL: the canonical read-back is `{}`.
            ("old", &[]),
        ]),
    });

    // `kind`: an IDENTITY key, a foreign key to `status`, under `ensure`.
    let mut kind = Table::default();
    let mut id = Column::new(ty("int")).not_null();
    id.identity = Some(Identity {
        seed: 1,
        increment: 1,
    });
    kind.columns.insert("id".to_owned(), id);
    kind.columns.insert(
        "status_code".to_owned(),
        Column::new(ty("varchar(20)")).not_null(),
    );
    kind.primary_key = Some(PrimaryKey {
        name: None,
        columns: vec!["id".to_owned()],
    });
    kind.foreign_keys.insert(
        "fk_kind_status".to_owned(),
        ForeignKey {
            columns: vec!["status_code".to_owned()],
            references_table: TableName::new("dbo", "status"),
            references_columns: vec!["code".to_owned()],
            on_delete: Default::default(),
            on_update: Default::default(),
        },
    );
    kind.data = Some(TableData {
        mode: DataMode::Ensure,
        rows: rows(&[("7", &[("status_code", text("old"))])]),
    });

    let mut declared = Schema::default();
    declared
        .tables
        .insert(TableName::new("dbo", "status"), status);
    declared.tables.insert(TableName::new("dbo", "kind"), kind);
    let ids = mint_ids(&declared, &IdsFile::default(), &[]);

    let mut db = TestDb::create("readback").await;
    apply(
        &mut db.conn,
        &plan(&Schema::default(), &IdsFile::default(), &declared, &ids),
    )
    .await;

    let scopes = declared.data_scopes();
    let read: std::collections::BTreeMap<TableName, RowScope> = scopes
        .iter()
        .map(|(n, s)| (n.clone(), s.rows_to_read()))
        .collect();
    let live = pbps_mssql::catalog::introspect(&mut db.conn)
        .await
        .expect("introspect")
        .schema;
    let observed = pbps_mssql::catalog::read_rows(&mut db.conn, &live, &read)
        .await
        .expect("read rows");
    let live = live
        .with_observed_rows(&observed, &scopes, &declared)
        .unwrap();

    // What was declared is what comes back — `1.50` as the engine spells a
    // decimal(5,2), the defaulted label omitted, the NULLs omitted.
    assert_eq!(
        live.tables[&TableName::new("dbo", "status")].data,
        declared.tables[&TableName::new("dbo", "status")].data
    );
    assert_eq!(
        live.tables[&TableName::new("dbo", "kind")].data,
        declared.tables[&TableName::new("dbo", "kind")].data
    );
    // The whole thing: the rows make no difference to the schema equality
    // that the drift check is built on.
    assert_eq!(
        live.tables[&TableName::new("dbo", "status")],
        normalized(&declared).tables[&TableName::new("dbo", "status")]
    );

    // Hand edits: a changed label, a NULL where the default was, a rogue row
    // in the `exact` table, and an application row in the `ensure` table.
    db.conn
        .execute(
            "UPDATE dbo.status SET label = N'Ancient', rank = 9 WHERE code = 'old';\n\
             INSERT INTO dbo.status (code) VALUES ('rogue');\n\
             INSERT INTO dbo.kind (status_code) VALUES ('new');",
        )
        .await
        .expect("hand edits");
    let again = pbps_mssql::catalog::introspect(&mut db.conn)
        .await
        .expect("introspect")
        .schema;
    let observed = pbps_mssql::catalog::read_rows(&mut db.conn, &again, &read)
        .await
        .expect("read rows");
    let again = again
        .with_observed_rows(&observed, &scopes, &declared)
        .unwrap();

    let status_rows = &again.tables[&TableName::new("dbo", "status")]
        .data
        .as_ref()
        .unwrap()
        .rows;
    assert_eq!(
        status_rows[&RowKey::from("old")],
        [
            ("label".to_owned(), text("Ancient")),
            ("rank".to_owned(), Value::Int(9))
        ]
        .into_iter()
        .collect::<Row>()
    );
    assert!(status_rows.contains_key(&RowKey::from("rogue")));
    // `ensure` reads its keys and nothing else: the application's own row is
    // invisible, exactly as the mode promises.
    let kind_rows = &again.tables[&TableName::new("dbo", "kind")]
        .data
        .as_ref()
        .unwrap()
        .rows;
    assert_eq!(kind_rows.len(), 1, "{kind_rows:?}");

    // As drift: the differ phrases the hand edits as changes from the recorded
    // state to the live one.
    let drift = pbps_diff::diff_partial(
        pbps_diff::Side {
            schema: &live,
            ids: &ids,
        },
        pbps_diff::Side {
            schema: &again,
            ids: &pbps_diff::observed_ids(&again, &ids),
        },
        &Mssql,
        &pbps_model::Hints::default(),
    );
    assert!(drift.errors.is_empty(), "{:?}", drift.errors);
    let described: Vec<String> = drift
        .changes
        .changes
        .iter()
        .map(|p| format!("{:?}", p.change))
        .collect();
    assert!(
        described
            .iter()
            .any(|d| d.starts_with("UpdateRow") && d.contains("old")),
        "{described:?}"
    );
    assert!(
        described
            .iter()
            .any(|d| d.starts_with("InsertRow") && d.contains("rogue")),
        "{described:?}"
    );
    assert_eq!(drift.changes.changes.len(), 2, "{described:?}");

    // The pre-delete probe, against the catalog: kind 7 points at `old`, so
    // deleting `old` is refused with a count of one — and a plan that moves
    // kind 7 first is not.
    let probe_for = |cs: &pbps_model::ChangeSet| {
        Mssql
            .preflight(cs)
            .into_iter()
            .find(|p| p.description.contains("row `old`"))
            .expect("the delete carries a probe")
    };
    let delete = pbps_model::PlannedChange::new(pbps_model::Change::DeleteRow {
        table: TableName::new("dbo", "status"),
        key_column: "code".to_owned(),
        key: RowKey::from("old"),
        cause: pbps_model::change::DeleteCause::Undeclared,
        row: std::collections::BTreeMap::new(),
        types: std::collections::BTreeMap::new(),
        after_types: Default::default(),
    });
    let alone = pbps_model::ChangeSet {
        changes: vec![delete.clone()],
    };
    let probe = probe_for(&alone);
    let n: i32 = db
        .conn
        .query(&probe.sql)
        .await
        .unwrap_or_else(|e| panic!("the engine rejected the probe:\n{}\n{e}", probe.sql))[0]
        .try_get_at(0)
        .unwrap()
        .unwrap();
    assert_eq!(n, 1, "kind 7 still points at `old`");

    // A foreign key this plan takes away first is not a reason to refuse the
    // delete: `DropForeignKey` and `DropTable` both sort before `DeleteRow`,
    // so by the time the delete runs the constraint is gone and the child
    // row references nothing (DECISIONS 128).
    for (removal, why) in [
        (
            pbps_model::Change::DropForeignKey {
                table: TableName::new("dbo", "kind"),
                name: "fk_kind_status".to_owned(),
            },
            "the constraint this plan drops first counts nothing",
        ),
        (
            pbps_model::Change::DropTable {
                uid: "t_bbbbbb".parse().unwrap(),
                name: TableName::new("dbo", "kind"),
            },
            "nor does a key held by a table this plan drops first",
        ),
    ] {
        let cs = pbps_model::ChangeSet {
            changes: vec![pbps_model::PlannedChange::new(removal), delete.clone()],
        };
        let probe = probe_for(&cs);
        let n: i32 = db
            .conn
            .query(&probe.sql)
            .await
            .unwrap_or_else(|e| panic!("the engine rejected the probe:\n{}\n{e}", probe.sql))[0]
            .try_get_at(0)
            .unwrap()
            .unwrap();
        assert_eq!(n, 0, "{why}");
    }
    // A different constraint of the same name on another table leaves this
    // one counted: the exclusion names the table too.
    let elsewhere = pbps_model::ChangeSet {
        changes: vec![
            pbps_model::PlannedChange::new(pbps_model::Change::DropForeignKey {
                table: TableName::new("other", "kind"),
                name: "fk_kind_status".to_owned(),
            }),
            delete.clone(),
        ],
    };
    let probe = probe_for(&elsewhere);
    let n: i32 = db
        .conn
        .query(&probe.sql)
        .await
        .unwrap_or_else(|e| panic!("the engine rejected the probe:\n{}\n{e}", probe.sql))[0]
        .try_get_at(0)
        .unwrap()
        .unwrap();
    assert_eq!(
        n, 1,
        "another table's constraint of the same name is not this one"
    );

    let moved = pbps_model::ChangeSet {
        changes: vec![
            pbps_model::PlannedChange::new(pbps_model::Change::UpdateRow {
                unchanged: Default::default(),
                types: Default::default(),
                after_types: Default::default(),
                table: TableName::new("dbo", "kind"),
                key_column: "id".to_owned(),
                key: RowKey::from("7"),
                columns: [(
                    "status_code".to_owned(),
                    (
                        pbps_model::Cell::Value(text("old")),
                        pbps_model::Cell::Value(text("new")),
                    ),
                )]
                .into_iter()
                .collect(),
            }),
            delete,
        ],
    };
    let probe = probe_for(&moved);
    let n: i32 = db
        .conn
        .query(&probe.sql)
        .await
        .unwrap_or_else(|e| panic!("the engine rejected the probe:\n{}\n{e}", probe.sql))[0]
        .try_get_at(0)
        .unwrap()
        .unwrap();
    assert_eq!(n, 0, "a row the plan moves is not counted");

    // An update that sets the referencing column to the *same* key under
    // another spelling — `OLD` for `old`, one key to a case-insensitive
    // collation — moves nothing, and the engine, not this crate, is the one
    // that knows. Counted.
    let same_key = pbps_model::ChangeSet {
        changes: vec![
            pbps_model::PlannedChange::new(pbps_model::Change::UpdateRow {
                unchanged: Default::default(),
                types: Default::default(),
                after_types: Default::default(),
                table: TableName::new("dbo", "kind"),
                key_column: "id".to_owned(),
                key: RowKey::from("7"),
                columns: [(
                    "status_code".to_owned(),
                    (
                        pbps_model::Cell::Value(text("old")),
                        pbps_model::Cell::Value(text("OLD")),
                    ),
                )]
                .into_iter()
                .collect(),
            }),
            moved.changes[1].clone(),
        ],
    };
    let probe = probe_for(&same_key);
    let n: i32 = db
        .conn
        .query(&probe.sql)
        .await
        .unwrap_or_else(|e| panic!("the engine rejected the probe:\n{}\n{e}", probe.sql))[0]
        .try_get_at(0)
        .unwrap()
        .unwrap();
    assert_eq!(
        n, 1,
        "`OLD` is `old` to the engine: the row still points at it"
    );

    // An update that sets some other column of kind 7 leaves it pointing at
    // `old`: still counted. The first cut excluded every updated row, and
    // under ON DELETE CASCADE that was a silent delete.
    db.conn
        .execute("ALTER TABLE dbo.kind ADD note nvarchar(10) NULL;")
        .await
        .expect("a column the update can set");
    let elsewhere = pbps_model::ChangeSet {
        changes: vec![
            pbps_model::PlannedChange::new(pbps_model::Change::UpdateRow {
                unchanged: Default::default(),
                types: Default::default(),
                after_types: Default::default(),
                table: TableName::new("dbo", "kind"),
                key_column: "id".to_owned(),
                key: RowKey::from("7"),
                columns: [(
                    "note".to_owned(),
                    (
                        pbps_model::Cell::Value(Value::Null),
                        pbps_model::Cell::Value(text("x")),
                    ),
                )]
                .into_iter()
                .collect(),
            }),
            moved.changes[1].clone(),
        ],
    };
    let probe = probe_for(&elsewhere);
    let n: i32 = db
        .conn
        .query(&probe.sql)
        .await
        .unwrap_or_else(|e| panic!("the engine rejected the probe:\n{}\n{e}", probe.sql))[0]
        .try_get_at(0)
        .unwrap()
        .unwrap();
    assert_eq!(n, 1, "a row updated elsewhere still points at `old`");

    // Rows this plan puts *onto* `old`. The count is of what is stored, and
    // inserts and updates both run before the deletes, so neither is visible
    // to a probe that only looks at the table: with ON DELETE CASCADE the
    // engine would take the new row straight back out, the apply would
    // succeed, and the next plan would propose it all over again.
    let insert_onto = |code: &str| pbps_model::ChangeSet {
        changes: vec![
            moved.changes[0].clone(),
            pbps_model::PlannedChange::new(pbps_model::Change::InsertRow {
                table: TableName::new("dbo", "kind"),
                key_column: "id".to_owned(),
                identity_key: true,
                key: RowKey::from("9"),
                defaults: Default::default(),
                types: Default::default(),
                row: [("status_code".to_owned(), text(code))]
                    .into_iter()
                    .collect::<Row>(),
            }),
            moved.changes[1].clone(),
        ],
    };
    // The same insert with the column left to its default: the plan carries
    // the default, and the engine compares it like any spelled value
    // (DECISIONS 117).
    let insert_defaulted = |default: &str| pbps_model::ChangeSet {
        changes: vec![
            moved.changes[0].clone(),
            pbps_model::PlannedChange::new(pbps_model::Change::InsertRow {
                table: TableName::new("dbo", "kind"),
                key_column: "id".to_owned(),
                identity_key: true,
                key: RowKey::from("9"),
                defaults: [("status_code".to_owned(), default.to_owned())]
                    .into_iter()
                    .collect(),
                types: Default::default(),
                row: Row::default(),
            }),
            moved.changes[1].clone(),
        ],
    };
    for (cs, want, why) in [
        (
            insert_onto("old"),
            1,
            "an inserted child pointing at `old` is counted, though it is not there yet",
        ),
        (
            insert_defaulted("('old')"),
            1,
            "a child left to a default of `old` lands on the row being deleted",
        ),
        (
            insert_defaulted("(N'OLD')"),
            1,
            "the engine reads the default, spelling and all",
        ),
        (
            insert_defaulted("('new')"),
            0,
            "a default naming another parent blocks nothing",
        ),
        // The engine decides which spellings are one key here too.
        (
            insert_onto("OLD"),
            1,
            "`OLD` is `old` to the engine: the insert lands on the row being deleted",
        ),
        (
            insert_onto("new"),
            0,
            "an insert onto another parent blocks nothing",
        ),
    ] {
        let probe = probe_for(&cs);
        let n: i32 = db
            .conn
            .query(&probe.sql)
            .await
            .unwrap_or_else(|e| panic!("the engine rejected the probe:\n{}\n{e}", probe.sql))[0]
            .try_get_at(0)
            .unwrap()
            .unwrap();
        assert_eq!(n, want, "{why}");
    }

    // A default the probe cannot evaluate — not a literal, though it may
    // well be the deleted key — was treated as no arrival, and the cascade
    // took the child. Refused now where the catalog says a foreign key to
    // the table spans the column, by a second probe (DECISIONS 124); a
    // column no key spans is nobody's business.
    let refusal_for = |cs: &pbps_model::ChangeSet| {
        Mssql
            .preflight(cs)
            .into_iter()
            .find(|p| p.description.contains("cannot evaluate"))
    };
    let unprobeable = insert_defaulted("(CONVERT(varchar(20), 'old'))");
    let probe = refusal_for(&unprobeable).expect("the write is refused, not dropped");
    let n: i32 = db
        .conn
        .query(&probe.sql)
        .await
        .unwrap_or_else(|e| panic!("the engine rejected the probe:\n{}\n{e}", probe.sql))[0]
        .try_get_at(0)
        .unwrap()
        .unwrap();
    assert_eq!(
        n, 1,
        "fk_kind_status spans status_code: {}",
        probe.description
    );
    assert!(
        probe.description.contains("status_code (row `9`)"),
        "{}",
        probe.description
    );
    // The same filtering the count does: a key this plan takes away before
    // the deletes cannot carry a default onto the deleted row, so refusing
    // the write for it would refuse a plan the engine accepts (128).
    let mut without_the_key = unprobeable.clone();
    without_the_key.changes.insert(
        0,
        pbps_model::PlannedChange::new(pbps_model::Change::DropForeignKey {
            table: TableName::new("dbo", "kind"),
            name: "fk_kind_status".to_owned(),
        }),
    );
    let probe = refusal_for(&without_the_key).expect("the probe is still built");
    let n: i32 = db
        .conn
        .query(&probe.sql)
        .await
        .unwrap_or_else(|e| panic!("the engine rejected the probe:\n{}\n{e}", probe.sql))[0]
        .try_get_at(0)
        .unwrap()
        .unwrap();
    assert_eq!(n, 0, "the only key spanning the column is going first");
    // And the whole child table going takes its keys with it.
    let mut without_the_table = unprobeable.clone();
    without_the_table.changes.insert(
        0,
        pbps_model::PlannedChange::new(pbps_model::Change::DropTable {
            uid: "t_cccccc".parse().unwrap(),
            name: TableName::new("dbo", "kind"),
        }),
    );
    assert!(
        refusal_for(&without_the_table).is_none(),
        "a table this plan drops first has nothing left to refuse"
    );

    let elsewhere = pbps_model::ChangeSet {
        changes: vec![
            moved.changes[0].clone(),
            pbps_model::PlannedChange::new(pbps_model::Change::InsertRow {
                table: TableName::new("dbo", "kind"),
                key_column: "id".to_owned(),
                identity_key: true,
                key: RowKey::from("9"),
                defaults: [("note".to_owned(), "(CONVERT(nvarchar(10), 'x'))".to_owned())]
                    .into_iter()
                    .collect(),
                types: Default::default(),
                row: [("status_code".to_owned(), text("new"))]
                    .into_iter()
                    .collect::<Row>(),
            }),
            moved.changes[1].clone(),
        ],
    };
    let probe = refusal_for(&elsewhere).expect("asked, since the plan cannot know the keys");
    let n: i32 = db
        .conn
        .query(&probe.sql)
        .await
        .unwrap_or_else(|e| panic!("the engine rejected the probe:\n{}\n{e}", probe.sql))[0]
        .try_get_at(0)
        .unwrap()
        .unwrap();
    assert_eq!(n, 0, "no key spans `note`");
    assert!(
        refusal_for(&insert_defaulted("(NULL)")).is_none(),
        "NULL names no row"
    );

    // The same shape through an update: a stored row pointing elsewhere that
    // this plan moves *onto* `old`. Found by sweeping the insert case, not
    // reported.
    db.conn
        .execute(
            "SET IDENTITY_INSERT dbo.kind ON;\n\
             INSERT INTO dbo.kind (id, status_code) VALUES (42, 'new');\n\
             SET IDENTITY_INSERT dbo.kind OFF;",
        )
        .await
        .expect("a child pointing somewhere else");
    let update_onto = |code: &str| pbps_model::ChangeSet {
        changes: vec![
            moved.changes[0].clone(),
            pbps_model::PlannedChange::new(pbps_model::Change::UpdateRow {
                unchanged: Default::default(),
                types: Default::default(),
                after_types: Default::default(),
                table: TableName::new("dbo", "kind"),
                key_column: "id".to_owned(),
                key: RowKey::from("42"),
                columns: [(
                    "status_code".to_owned(),
                    (
                        pbps_model::Cell::Value(text("new")),
                        pbps_model::Cell::Value(text(code)),
                    ),
                )]
                .into_iter()
                .collect(),
            }),
            moved.changes[1].clone(),
        ],
    };
    for (cs, want, why) in [
        (
            update_onto("old"),
            1,
            "a row moved onto `old` is counted, though it is stored elsewhere",
        ),
        (
            update_onto("rogue"),
            0,
            "a row moved onto another parent blocks nothing",
        ),
    ] {
        let probe = probe_for(&cs);
        let n: i32 = db
            .conn
            .query(&probe.sql)
            .await
            .unwrap_or_else(|e| panic!("the engine rejected the probe:\n{}\n{e}", probe.sql))[0]
            .try_get_at(0)
            .unwrap()
            .unwrap();
        assert_eq!(n, want, "{why}");
    }

    // A foreign key need not target the primary key. The probe filtered the
    // catalog to `rc.name = <the key column>`, so a child referencing some
    // other unique key of the same parent row was left out of the query
    // altogether — and `ON DELETE CASCADE` would take it, unreported, in an
    // unmanaged application table as easily as a declared one.
    // One batch per statement: SQL Server compiles a whole batch before it
    // runs any of it, so a column added and used in the same one is
    // "Invalid column name".
    for sql in [
        "ALTER TABLE dbo.status ADD alt int NULL;",
        "UPDATE dbo.status SET alt = CASE code WHEN 'old' THEN 1 WHEN 'new' THEN 2 ELSE 3 END;",
        "ALTER TABLE dbo.status ADD CONSTRAINT uq_status_alt UNIQUE (alt);",
        "CREATE TABLE dbo.alt_child (\n\
             id int NOT NULL CONSTRAINT pk_alt_child PRIMARY KEY,\n\
             alt int NULL CONSTRAINT fk_alt_child REFERENCES dbo.status (alt) ON DELETE CASCADE\n\
         );",
        "INSERT INTO dbo.alt_child (id, alt) VALUES (1, 1), (2, 2);",
    ] {
        db.conn
            .execute(sql)
            .await
            .unwrap_or_else(|e| panic!("an alternate key and a child for it:\n{sql}\n{e}"));
    }
    let probe = probe_for(&moved);
    let n: i32 = db
        .conn
        .query(&probe.sql)
        .await
        .unwrap_or_else(|e| panic!("the engine rejected the probe:\n{}\n{e}", probe.sql))[0]
        .try_get_at(0)
        .unwrap()
        .unwrap();
    assert_eq!(
        n, 1,
        "alt_child 1 references `old` through uq_status_alt, and the delete would cascade into it"
    );
    // And the child that references a *different* parent row through the
    // same alternate key is not counted: the probe reads the row being
    // deleted, not the constraint.
    db.conn
        .execute("DELETE FROM dbo.alt_child WHERE id = 1;")
        .await
        .expect("leave only the child of another row");
    let probe = probe_for(&moved);
    let n: i32 = db
        .conn
        .query(&probe.sql)
        .await
        .unwrap_or_else(|e| panic!("the engine rejected the probe:\n{}\n{e}", probe.sql))[0]
        .try_get_at(0)
        .unwrap()
        .unwrap();
    assert_eq!(n, 0, "alt_child 2 references `new`, which is not going");
    db.conn
        .execute("DROP TABLE dbo.alt_child;")
        .await
        .expect("out of the way of the checks below");

    // A foreign key is a tuple. Counted per column, a child on the surviving
    // parent `(1, 2)` matched the deleted `(1, 1)` by its first column, and
    // every delete of a row with a composite alternate key was refused
    // (DECISIONS 121). `(grp, sub)`: old (1, 1), new (1, 2), rogue (2, 1).
    for sql in [
        "ALTER TABLE dbo.status ADD grp int NULL, sub int NULL;",
        "UPDATE dbo.status SET grp = CASE code WHEN 'rogue' THEN 2 ELSE 1 END, \
         sub = CASE code WHEN 'new' THEN 2 ELSE 1 END;",
        "ALTER TABLE dbo.status ADD CONSTRAINT uq_status_pair UNIQUE (grp, sub);",
        "CREATE TABLE dbo.pair_child (\n\
             id int NOT NULL CONSTRAINT pk_pair_child PRIMARY KEY,\n\
             grp int NULL,\n\
             sub int NULL,\n\
             CONSTRAINT fk_pair_child FOREIGN KEY (grp, sub) REFERENCES dbo.status (grp, sub) \
         ON DELETE CASCADE\n\
         );",
        "INSERT INTO dbo.pair_child (id, grp, sub) VALUES (1, 1, 1), (2, 1, 2), (3, 2, 1), (4, 1, NULL);",
    ] {
        db.conn.execute(sql).await.unwrap_or_else(|e| {
            panic!("a composite alternate key and children for it:\n{sql}\n{e}")
        });
    }
    let pair_update = |id: &str, cells: &[(&str, Value)]| {
        pbps_model::PlannedChange::new(pbps_model::Change::UpdateRow {
            unchanged: Default::default(),
            types: Default::default(),
            after_types: Default::default(),
            table: TableName::new("dbo", "pair_child"),
            key_column: "id".to_owned(),
            key: RowKey::from(id),
            columns: cells
                .iter()
                .map(|(c, v)| {
                    (
                        (*c).to_owned(),
                        (
                            pbps_model::Cell::Value(Value::Null),
                            pbps_model::Cell::Value(v.clone()),
                        ),
                    )
                })
                .collect(),
        })
    };
    let pair_insert = |id: &str, cells: &[(&str, Value)]| {
        pbps_model::PlannedChange::new(pbps_model::Change::InsertRow {
            table: TableName::new("dbo", "pair_child"),
            key_column: "id".to_owned(),
            identity_key: false,
            key: RowKey::from(id),
            defaults: Default::default(),
            types: Default::default(),
            row: cells
                .iter()
                .map(|(c, v)| ((*c).to_owned(), v.clone()))
                .collect::<Row>(),
        })
    };
    let with = |change: pbps_model::PlannedChange| pbps_model::ChangeSet {
        changes: vec![moved.changes[0].clone(), change, moved.changes[1].clone()],
    };
    let int = Value::Int;
    for (cs, want, why) in [
        (
            pbps_model::ChangeSet {
                changes: moved.changes.clone(),
            },
            1,
            "pair_child 1 on (1, 1) references `old`; 2 on (1, 2), 3 on (2, 1) and 4 on (1, NULL) do not",
        ),
        (
            with(pair_update("1", &[("sub", int(2))])),
            0,
            "pair_child 1 moved to (1, 2) is not counted",
        ),
        (
            with(pair_update("1", &[("grp", int(2)), ("sub", int(1))])),
            0,
            "an update setting both columns is one tuple: (2, 1) is `rogue`",
        ),
        (
            with(pair_update("1", &[("sub", Value::Null)])),
            1,
            "a cell set to NULL is not compared: the row stays counted",
        ),
        (
            with(pair_update("2", &[("sub", int(1))])),
            2,
            "pair_child 2 moved onto (1, 1) is counted beside 1",
        ),
        (
            with(pair_update("3", &[("grp", int(1))])),
            2,
            "pair_child 3 moved onto (1, 1) by its first column is counted",
        ),
        (
            with(pair_update("4", &[("sub", int(1))])),
            2,
            "pair_child 4 references nothing while `sub` is NULL, and is counted once set onto (1, 1)",
        ),
        (
            with(pair_insert("5", &[("grp", int(1)), ("sub", int(1))])),
            2,
            "an inserted child on (1, 1) is counted",
        ),
        (
            with(pair_insert("5", &[("grp", int(1)), ("sub", int(2))])),
            1,
            "an inserted child on (1, 2) is not",
        ),
        (
            with(pair_insert("5", &[("grp", int(1))])),
            1,
            "an inserted child with `sub` left to NULL references nothing",
        ),
    ] {
        let probe = probe_for(&cs);
        let n: i32 = db
            .conn
            .query(&probe.sql)
            .await
            .unwrap_or_else(|e| panic!("the engine rejected the probe:\n{}\n{e}", probe.sql))[0]
            .try_get_at(0)
            .unwrap()
            .unwrap();
        assert_eq!(n, want, "{why}");
    }
    db.conn
        .execute("DROP TABLE dbo.pair_child;")
        .await
        .expect("out of the way of the checks below");

    // A table that has lost its key is unreadable, not empty.
    db.conn
        .execute("ALTER TABLE dbo.kind DROP CONSTRAINT fk_kind_status; DECLARE @pk sysname = (SELECT name FROM sys.key_constraints WHERE parent_object_id = OBJECT_ID('dbo.status') AND type = 'PK'); EXEC('ALTER TABLE dbo.status DROP CONSTRAINT ' + @pk);")
        .await
        .expect("drop the key");
    let keyless = pbps_mssql::catalog::introspect(&mut db.conn)
        .await
        .expect("introspect")
        .schema;
    let err = pbps_mssql::catalog::read_rows(&mut db.conn, &keyless, &read)
        .await
        .expect_err("rows without a key cannot be read");
    assert!(err.to_string().contains("dbo.status"), "{err}");

    db.drop().await;
}

/// `NOCHECK CONSTRAINT` leaves a foreign key in the catalog and stops the
/// engine enforcing it. Measured here rather than assumed: with the
/// constraint disabled the parent delete succeeds, the child row stays, and
/// the `ON DELETE CASCADE` does not run — so counting those children refused
/// a delete the engine allows, and refused it for ever (DECISIONS 144).
#[tokio::test]
#[ignore = "needs a SQL Server; see scripts/live-tests.sh"]
async fn a_disabled_foreign_key_neither_cascades_nor_blocks_a_delete() {
    use pbps_model::RowKey;

    let mut db = TestDb::create("nocheck").await;
    for sql in [
        "CREATE TABLE dbo.status (code varchar(10) NOT NULL CONSTRAINT pk_status PRIMARY KEY);",
        "CREATE TABLE dbo.kind (\n\
             id int NOT NULL CONSTRAINT pk_kind PRIMARY KEY,\n\
             status_code varchar(10) NULL CONSTRAINT fk_kind_status\n\
                 REFERENCES dbo.status (code) ON DELETE CASCADE\n\
         );",
        "INSERT INTO dbo.status (code) VALUES ('old');",
        "INSERT INTO dbo.kind (id, status_code) VALUES (1, 'old');",
        "ALTER TABLE dbo.kind NOCHECK CONSTRAINT fk_kind_status;",
    ] {
        db.conn
            .execute(sql)
            .await
            .unwrap_or_else(|e| panic!("{sql}\n{e}"));
    }

    let delete = pbps_model::Change::DeleteRow {
        table: TableName::new("dbo", "status"),
        key_column: "code".to_owned(),
        key: RowKey::from("old"),
        cause: pbps_model::change::DeleteCause::Undeclared,
        row: std::collections::BTreeMap::new(),
        types: std::collections::BTreeMap::new(),
        after_types: Default::default(),
    };
    let cs = pbps_model::ChangeSet {
        changes: vec![pbps_model::PlannedChange::new(delete.clone())],
    };

    // The probe counts nothing: the child is there, but nothing enforces the
    // constraint that would take it.
    let probe = Mssql
        .preflight(&cs)
        .into_iter()
        .find(|p| p.description.contains("row `old`"))
        .expect("the delete carries a probe");
    let n: i32 = db.conn.query(&probe.sql).await.expect("probe")[0]
        .try_get_at(0)
        .unwrap()
        .unwrap();
    assert_eq!(
        n, 0,
        "a disabled foreign key is not one the engine enforces"
    );

    // And the delete's own guard agrees, so the statement runs.
    let stmts = Mssql.emit(&delete, Default::default()).expect("emit");
    db.conn
        .execute(&stmts[0].sql)
        .await
        .expect("the engine allows this delete, so pbps must too");

    // What the engine actually did, measured rather than assumed: the child
    // row is still there, uncascaded.
    let left: i32 = db
        .conn
        .query("SELECT COUNT(*) FROM dbo.kind;")
        .await
        .expect("count")[0]
        .try_get_at(0)
        .unwrap()
        .unwrap();
    assert_eq!(left, 1, "a disabled cascade does not run");

    db.drop().await;
}

/// The baseline checksum pins the state up to the moment `apply` reads it,
/// and the delete runs later still. A row an application session rewrote in
/// between was, until the delete carried the recorded row in its predicate,
/// removed as if it were the reviewed one — `@@ROWCOUNT = 1` and all
/// (DECISIONS 143).
#[tokio::test]
#[ignore = "needs a SQL Server; see scripts/live-tests.sh"]
async fn a_row_rewritten_after_the_plan_was_made_is_not_deleted_as_the_reviewed_one() {
    use pbps_model::{Cell, RowKey, Value};

    let mut db = TestDb::create("late_write").await;
    for sql in [
        "CREATE TABLE dbo.status (\n\
             code varchar(10) NOT NULL CONSTRAINT pk_status PRIMARY KEY,\n\
             label nvarchar(50) NULL,\n\
             rank int NULL\n\
         );",
        "INSERT INTO dbo.status (code, label, rank) VALUES ('old', N'Old', 1);",
    ] {
        db.conn
            .execute(sql)
            .await
            .unwrap_or_else(|e| panic!("{sql}\n{e}"));
    }

    let delete = pbps_model::Change::DeleteRow {
        table: TableName::new("dbo", "status"),
        key_column: "code".to_owned(),
        key: RowKey::from("old"),
        cause: pbps_model::change::DeleteCause::Undeclared,
        row: [
            (
                "label".to_owned(),
                Cell::Value(Value::Text("Old".to_owned())),
            ),
            ("rank".to_owned(), Cell::Value(Value::Int(1))),
        ]
        .into_iter()
        .collect(),
        types: [
            ("label".to_owned(), ty("nvarchar")),
            ("rank".to_owned(), ty("int")),
        ]
        .into_iter()
        .collect(),
        after_types: Default::default(),
    };
    let sql = Mssql.emit(&delete, Default::default()).expect("emit")[0]
        .sql
        .clone();

    // Another connection, as an application would: the row is rewritten
    // after the plan was reviewed and before the delete runs.
    let mut other = connect_live(&conn_str()).await.expect("second connection");
    other
        .execute(&format!("USE [{}];", db.name))
        .await
        .expect("use");
    other
        .execute("UPDATE dbo.status SET label = N'Renamed by the application' WHERE code = 'old';")
        .await
        .expect("an application rewrites the row");

    let err = db
        .conn
        .execute(&sql)
        .await
        .expect_err("the delete refuses a row that is not the one reviewed");
    assert!(
        err.to_string()
            .contains("changed or deleted since the plan was made"),
        "{err}"
    );

    let mut c = connect_live(&conn_str()).await.expect("connect");
    c.execute(&format!("USE [{}];", db.name))
        .await
        .expect("use");
    let n: i32 = c
        .query("SELECT COUNT(*) FROM dbo.status WHERE code = 'old';")
        .await
        .expect("count")[0]
        .try_get_at(0)
        .unwrap()
        .unwrap();
    assert_eq!(n, 1, "the application's row is still there");

    // Put it back as the plan recorded it, and the same statement deletes it.
    c.execute("UPDATE dbo.status SET label = N'Old' WHERE code = 'old';")
        .await
        .expect("restore");
    db.conn
        .execute(&sql)
        .await
        .expect("the reviewed row is deleted");
    let n: i32 = c
        .query("SELECT COUNT(*) FROM dbo.status WHERE code = 'old';")
        .await
        .expect("count")[0]
        .try_get_at(0)
        .unwrap()
        .unwrap();
    assert_eq!(n, 0, "and it is gone");

    db.drop().await;
}

/// The preflight probe counts before the first statement; the delete runs
/// later. A child row committed in between was, until the delete carried a
/// guard of its own, taken silently by `ON DELETE CASCADE` — and the closing
/// snapshot then recorded the damage as a success (DECISIONS 129).
#[tokio::test]
#[ignore = "needs a SQL Server; see scripts/live-tests.sh"]
async fn a_child_row_that_arrives_after_the_probe_is_not_cascaded_away() {
    use pbps_model::RowKey;

    let mut db = TestDb::create("late_child").await;
    for sql in [
        "CREATE TABLE dbo.status (code varchar(10) NOT NULL CONSTRAINT pk_status PRIMARY KEY);",
        "CREATE TABLE dbo.kind (\n\
             id int NOT NULL CONSTRAINT pk_kind PRIMARY KEY,\n\
             status_code varchar(10) NULL CONSTRAINT fk_kind_status\n\
                 REFERENCES dbo.status (code) ON DELETE CASCADE\n\
         );",
        "INSERT INTO dbo.status (code) VALUES ('old');",
    ] {
        db.conn
            .execute(sql)
            .await
            .unwrap_or_else(|e| panic!("{sql}\n{e}"));
    }

    let delete = pbps_model::Change::DeleteRow {
        table: TableName::new("dbo", "status"),
        key_column: "code".to_owned(),
        key: RowKey::from("old"),
        cause: pbps_model::change::DeleteCause::Undeclared,
        row: std::collections::BTreeMap::new(),
        types: std::collections::BTreeMap::new(),
        after_types: Default::default(),
    };
    let cs = pbps_model::ChangeSet {
        changes: vec![pbps_model::PlannedChange::new(delete.clone())],
    };

    // The probe, run where `apply` runs it: nothing references the row.
    let probe = Mssql
        .preflight(&cs)
        .into_iter()
        .find(|p| p.description.contains("row `old`"))
        .expect("the delete carries a probe");
    let n: i32 = db.conn.query(&probe.sql).await.expect("probe")[0]
        .try_get_at(0)
        .unwrap()
        .unwrap();
    assert_eq!(n, 0, "the probe sees no child, and the plan is approved");

    // Another connection, as an application would: a child row arrives and
    // commits between the probe and the delete.
    let mut other = connect_live(&conn_str()).await.expect("second connection");
    other
        .execute(&format!("USE [{}];", db.name))
        .await
        .expect("use");
    other
        .execute("INSERT INTO dbo.kind (id, status_code) VALUES (1, 'old');")
        .await
        .expect("a child arrives after the probe");

    let stmts = Mssql.emit(&delete, Default::default()).expect("emit");
    assert_eq!(stmts.len(), 1);
    let err = db
        .conn
        .execute(&stmts[0].sql)
        .await
        .expect_err("the delete refuses what the probe never saw");
    assert!(
        err.to_string()
            .contains("arrived after this plan was checked"),
        "{err}"
    );

    let count = |sql: &'static str| async {
        let mut c = connect_live(&conn_str()).await.expect("connect");
        c.execute(&format!("USE [{}];", db.name))
            .await
            .expect("use");
        let n: i32 = c.query(sql).await.expect("count")[0]
            .try_get_at(0)
            .unwrap()
            .unwrap();
        n
    };
    assert_eq!(
        count("SELECT COUNT(*) FROM dbo.kind;").await,
        1,
        "the child is still there, not cascaded away"
    );
    assert_eq!(
        count("SELECT COUNT(*) FROM dbo.status WHERE code = 'old';").await,
        1,
        "and so is the row the delete was about"
    );
    // The failed guard leaves no transaction open on the connection: a
    // staged apply runs each statement outside one, and an abandoned
    // `BEGIN TRANSACTION` would hold its locks until the process ended.
    let open: i32 = db
        .conn
        .query("SELECT @@TRANCOUNT;")
        .await
        .expect("trancount")[0]
        .try_get_at(0)
        .unwrap()
        .unwrap();
    assert_eq!(open, 0, "the CATCH rolled its own transaction back");

    // With the child gone the same statement runs, so the guard refuses only
    // what it has to.
    other
        .execute("DELETE FROM dbo.kind;")
        .await
        .expect("take the child away");
    db.conn
        .execute(&stmts[0].sql)
        .await
        .expect("nothing references the row now");
    assert_eq!(
        count("SELECT COUNT(*) FROM dbo.status WHERE code = 'old';").await,
        0,
        "the delete went through"
    );

    db.drop().await;
}

/// Whether two declared keys are one row is the *key column's* question.
/// The collision query compares `VALUES` literals, which carry the database's
/// default collation, so a column collated differently was answered about a
/// collation that is not its own — in one direction refusing two valid keys,
/// in the other letting two spellings of one row through to a pair of inserts
/// the primary key refuses (DECISIONS 131).
#[tokio::test]
#[ignore = "needs a SQL Server; see scripts/live-tests.sh"]
async fn key_collisions_are_judged_by_the_key_column_s_own_collation() {
    use pbps_model::{DataMode, Row, RowKey, TableData, Value};

    let mut db = TestDb::create("keycollation").await;
    // A case-sensitive database, so the column's collation and the
    // database's disagree in both directions below.
    db.conn
        .execute(&format!(
            "USE master; ALTER DATABASE [{0}] COLLATE Latin1_General_CS_AS; USE [{0}];",
            db.name
        ))
        .await
        .expect("a case-sensitive database");
    for sql in [
        // Case-insensitive column in a case-sensitive database: `a` and `A`
        // are one row here, and the old query said they were two.
        "CREATE TABLE dbo.ci (\n\
             code varchar(10) COLLATE Latin1_General_CI_AS NOT NULL CONSTRAINT pk_ci PRIMARY KEY,\n\
             label nvarchar(50) NOT NULL\n\
         );",
        // And one that takes the database's own collation: two rows.
        "CREATE TABLE dbo.cs (\n\
             code varchar(10) NOT NULL CONSTRAINT pk_cs PRIMARY KEY,\n\
             label nvarchar(50) NOT NULL\n\
         );",
    ] {
        db.conn
            .execute(sql)
            .await
            .unwrap_or_else(|e| panic!("{sql}\n{e}"));
    }

    let declared_with = |name: &TableName| {
        let mut t = Table::default();
        t.columns
            .insert("code".to_owned(), Column::new(ty("varchar(10)")).not_null());
        t.columns.insert(
            "label".to_owned(),
            Column::new(ty("nvarchar(50)")).not_null(),
        );
        t.primary_key = Some(PrimaryKey {
            name: None,
            columns: vec!["code".to_owned()],
        });
        t.data = Some(TableData {
            mode: DataMode::Exact,
            rows: ["a", "A"]
                .into_iter()
                .map(|k| {
                    (
                        RowKey::from(k),
                        [("label".to_owned(), Value::Text(format!("row {k}")))]
                            .into_iter()
                            .collect::<Row>(),
                    )
                })
                .collect(),
        });
        let mut schema = Schema::default();
        schema.tables.insert(name.clone(), t);
        schema
    };

    let ci = TableName::new("dbo", "ci");
    let conflicts =
        pbps_mssql::catalog::misspelt(&mut db.conn, &declared_with(&ci), &Default::default())
            .await
            .expect("ask the engine")
            .conflicts;
    assert_eq!(
        conflicts.len(),
        1,
        "`a` and `A` are one row to a case-insensitive column: {conflicts:?}"
    );
    assert_eq!(conflicts[0].table, ci);

    let cs = TableName::new("dbo", "cs");
    let conflicts =
        pbps_mssql::catalog::misspelt(&mut db.conn, &declared_with(&cs), &Default::default())
            .await
            .expect("ask the engine")
            .conflicts;
    assert!(
        conflicts.is_empty(),
        "a case-sensitive column holds both: {conflicts:?}"
    );

    // A table this plan has yet to create has no collation to read, and its
    // column will be made with the database's — which is this database's
    // case-sensitive default, so the two keys stand.
    let conflicts = pbps_mssql::catalog::misspelt(
        &mut db.conn,
        &declared_with(&TableName::new("dbo", "not_yet")),
        &Default::default(),
    )
    .await
    .expect("ask the engine")
    .conflicts;
    assert!(
        conflicts.is_empty(),
        "a table with no column yet answers under the database's collation: {conflicts:?}"
    );

    // And the other direction, where the old query refused two valid keys:
    // a case-sensitive column in a database whose default is not.
    let mut ci_db = TestDb::create("keycollation_ci").await;
    ci_db
        .conn
        .execute(
            "CREATE TABLE dbo.cs (\n\
                 code varchar(10) COLLATE Latin1_General_CS_AS NOT NULL\n\
                     CONSTRAINT pk_cs2 PRIMARY KEY,\n\
                 label nvarchar(50) NOT NULL\n\
             );",
        )
        .await
        .expect("a case-sensitive column");
    let conflicts =
        pbps_mssql::catalog::misspelt(&mut ci_db.conn, &declared_with(&cs), &Default::default())
            .await
            .expect("ask the engine")
            .conflicts;
    assert!(
        conflicts.is_empty(),
        "the column holds `a` and `A` apart, whatever the database does: {conflicts:?}"
    );
    ci_db.drop().await;

    // And under the names the catalog has *now*: the checks run before the
    // plan does, so a table or key column this revision renames is still
    // spelt the old way. Asked under the declared names there is no such
    // table, the collation read finds nothing, and the comparison falls back
    // to this case-sensitive database's default — letting through two
    // spellings that the case-insensitive column will refuse as one
    // (DECISIONS 148).
    let renamed = TableName::new("dbo", "ci_renamed");
    let at: pbps_mssql::rows::CatalogNames = [(
        renamed.clone(),
        pbps_mssql::rows::Catalogued {
            table: Some(ci.clone()),
            key_column: Some("code".to_owned()),
        },
    )]
    .into_iter()
    .collect();
    let mut declared_renamed = declared_with(&renamed);
    let t = declared_renamed
        .tables
        .get_mut(&renamed)
        .expect("the declared table");
    let held = t.columns.shift_remove("code").expect("the key column");
    t.columns.insert("key_code".to_owned(), held);
    t.primary_key = Some(PrimaryKey {
        name: None,
        columns: vec!["key_code".to_owned()],
    });

    let conflicts = pbps_mssql::catalog::misspelt(&mut db.conn, &declared_renamed, &at)
        .await
        .expect("ask the engine")
        .conflicts;
    assert_eq!(
        conflicts.len(),
        1,
        "the collation is the renamed column's own, read under the name the catalog still has: \
         {conflicts:?}"
    );
    let missed =
        pbps_mssql::catalog::misspelt(&mut db.conn, &declared_renamed, &Default::default())
            .await
            .expect("ask the engine")
            .conflicts;
    assert!(
        missed.is_empty(),
        "and under the declared names there is no column to read a collation from, so the \
         database's own answers instead — the miss this mapping exists to close: {missed:?}"
    );

    db.drop().await;
}

/// The engine reporting a successful write is not the same as the row being
/// what the plan says. An `AFTER` trigger runs inside the statement, and one
/// that rewrites the row, or puts a deleted one back, left `apply` reading
/// the result back, recording it, and reporting success — `verify` then clean
/// against a state nobody declared, and every plan after it proposing the
/// same change (DECISIONS 132).
#[tokio::test]
#[ignore = "needs a SQL Server; see scripts/live-tests.sh"]
async fn a_trigger_that_undoes_a_row_write_rolls_the_statement_back() {
    use pbps_model::{Cell, Row, RowKey, Value};

    let mut db = TestDb::create("triggers").await;
    for sql in [
        "CREATE TABLE dbo.status (\n\
             code varchar(10) NOT NULL CONSTRAINT pk_status PRIMARY KEY,\n\
             label nvarchar(50) NULL\n\
         );",
        "INSERT INTO dbo.status (code, label) VALUES ('old', 'Old');",
    ] {
        db.conn
            .execute(sql)
            .await
            .unwrap_or_else(|e| panic!("{sql}\n{e}"));
    }

    let table = TableName::new("dbo", "status");
    let insert = |key: &str| pbps_model::Change::InsertRow {
        table: table.clone(),
        key_column: "code".to_owned(),
        identity_key: false,
        key: RowKey::from(key),
        defaults: Default::default(),
        types: Default::default(),
        row: [("label".to_owned(), Value::Text("New".to_owned()))]
            .into_iter()
            .collect::<Row>(),
    };
    let update = |from: &str, to: &str| pbps_model::Change::UpdateRow {
        table: table.clone(),
        key_column: "code".to_owned(),
        key: RowKey::from("old"),
        columns: [(
            "label".to_owned(),
            (
                Cell::Value(Value::Text(from.to_owned())),
                Cell::Value(Value::Text(to.to_owned())),
            ),
        )]
        .into_iter()
        .collect(),
        unchanged: Default::default(),
        types: [("label".to_owned(), ty("nvarchar(50)"))]
            .into_iter()
            .collect(),
        after_types: Default::default(),
    };
    let delete = pbps_model::Change::DeleteRow {
        table: table.clone(),
        key_column: "code".to_owned(),
        key: RowKey::from("old"),
        cause: pbps_model::change::DeleteCause::Undeclared,
        row: std::collections::BTreeMap::new(),
        types: std::collections::BTreeMap::new(),
        after_types: Default::default(),
    };
    let sql_of = |change: &pbps_model::Change| {
        let stmts = Mssql.emit(change, Default::default()).expect("emit");
        assert_eq!(stmts.len(), 1, "{stmts:?}");
        stmts[0].sql.clone()
    };
    let db_name = db.name.clone();
    let label_of = |code: &'static str| {
        let db_name = db_name.clone();
        async move {
            let mut c = connect_live(&conn_str()).await.expect("connect");
            c.execute(&format!("USE [{db_name}];")).await.expect("use");
            let rows = c
                .query(&format!(
                    "SELECT label FROM dbo.status WHERE code = '{code}';"
                ))
                .await
                .expect("read the row");
            rows.first()
                .map(|r| r.try_get::<&str>("label").unwrap().unwrap().to_owned())
        }
    };

    // Nothing in the way: the postcondition refuses only what it has to.
    for (change, why) in [
        (insert("new"), "an ordinary insert"),
        (update("Old", "Ancient"), "an ordinary update"),
    ] {
        let sql = sql_of(&change);
        db.conn
            .execute(&sql)
            .await
            .unwrap_or_else(|e| panic!("{why} is refused:\n{sql}\n{e}"));
    }

    // A trigger that quietly rewrites whatever was just written.
    db.conn
        .execute(
            "CREATE TRIGGER dbo.undo ON dbo.status AFTER INSERT, UPDATE AS\n\
             BEGIN\n\
               SET NOCOUNT ON;\n\
               UPDATE dbo.status SET label = N'Undone'\n\
                WHERE code IN (SELECT code FROM inserted);\n\
             END;",
        )
        .await
        .expect("a trigger that rewrites what was written");

    for (change, why) in [
        (update("Ancient", "Newer"), "an update the trigger rewrote"),
        (insert("third"), "an insert the trigger rewrote"),
    ] {
        let sql = sql_of(&change);
        let err = db
            .conn
            .execute(&sql)
            .await
            .err()
            .unwrap_or_else(|| panic!("{why} was taken for this plan's own result:\n{sql}"));
        assert!(
            err.to_string().contains("is not what this plan wrote"),
            "{why}: {err}"
        );
    }
    assert_eq!(
        label_of("old").await.as_deref(),
        Some("Ancient"),
        "the update rolled back, trigger and all"
    );
    assert_eq!(
        label_of("third").await,
        None,
        "and so did the insert: the row is not there"
    );
    // The connection is left clean: a staged apply runs each statement
    // outside a transaction, and an abandoned one would hold its locks.
    let open: i32 = db
        .conn
        .query("SELECT @@TRANCOUNT;")
        .await
        .expect("trancount")[0]
        .try_get_at(0)
        .unwrap()
        .unwrap();
    assert_eq!(open, 0, "the CATCH rolled its own transaction back");

    // A column the row left to the table is held to that default too: the
    // trigger rewrites `note`, which the insert never names (DECISIONS 133).
    db.conn
        .execute("ALTER TABLE dbo.status ADD note nvarchar(50) NULL CONSTRAINT df_note DEFAULT N'plain';")
        .await
        .expect("a defaulted column the insert omits");
    db.conn
        .execute("DROP TRIGGER dbo.undo;")
        .await
        .expect("take the rewriting trigger off");
    db.conn
        .execute(
            "CREATE TRIGGER dbo.rewrite_note ON dbo.status AFTER INSERT AS\n\
             BEGIN\n\
               SET NOCOUNT ON;\n\
               UPDATE dbo.status SET note = N'rewritten'\n\
                WHERE code IN (SELECT code FROM inserted);\n\
             END;",
        )
        .await
        .expect("a trigger that rewrites a defaulted column");
    let defaulted = pbps_model::Change::InsertRow {
        table: table.clone(),
        key_column: "code".to_owned(),
        identity_key: false,
        key: RowKey::from("fourth"),
        defaults: [("note".to_owned(), "(N'plain')".to_owned())]
            .into_iter()
            .collect(),
        types: [("note".to_owned(), ty("nvarchar(50)"))]
            .into_iter()
            .collect(),
        row: [("label".to_owned(), Value::Text("New".to_owned()))]
            .into_iter()
            .collect::<Row>(),
    };
    let sql = sql_of(&defaulted);
    let err = db
        .conn
        .execute(&sql)
        .await
        .err()
        .unwrap_or_else(|| panic!("a rewritten default was taken for the plan's own:\n{sql}"));
    assert!(
        err.to_string().contains("is not what this plan wrote"),
        "{err}"
    );
    assert_eq!(
        label_of("fourth").await,
        None,
        "the insert rolled back, default and all"
    );
    db.conn
        .execute("DROP TRIGGER dbo.rewrite_note;")
        .await
        .expect("and without the trigger the same insert goes in");
    db.conn.execute(&sql).await.expect("the ordinary case");

    // A column the table gives no default is left at NULL, and the row is
    // held to that: the trigger writes `memo`, which the insert never names
    // and no default touches (DECISIONS 136).
    db.conn
        .execute("ALTER TABLE dbo.status ADD memo nvarchar(50) NULL;")
        .await
        .expect("a column with no default the insert omits");
    db.conn
        .execute(
            "CREATE TRIGGER dbo.write_memo ON dbo.status AFTER INSERT AS\n\
             BEGIN\n\
               SET NOCOUNT ON;\n\
               UPDATE dbo.status SET memo = N'written'\n\
                WHERE code IN (SELECT code FROM inserted);\n\
             END;",
        )
        .await
        .expect("a trigger that fills a column left to nothing");
    let left_null = pbps_model::Change::InsertRow {
        table: table.clone(),
        key_column: "code".to_owned(),
        identity_key: false,
        key: RowKey::from("fifth"),
        defaults: [("note".to_owned(), "(N'plain')".to_owned())]
            .into_iter()
            .collect(),
        types: [
            ("note".to_owned(), ty("nvarchar(50)")),
            ("memo".to_owned(), ty("nvarchar(50)")),
        ]
        .into_iter()
        .collect(),
        row: [("label".to_owned(), Value::Text("New".to_owned()))]
            .into_iter()
            .collect::<Row>(),
    };
    let sql = sql_of(&left_null);
    let err = db
        .conn
        .execute(&sql)
        .await
        .err()
        .unwrap_or_else(|| panic!("a filled-in NULL was taken for the plan's own:\n{sql}"));
    assert!(
        err.to_string().contains("is not what this plan wrote"),
        "{err}"
    );
    assert_eq!(label_of("fifth").await, None, "the insert rolled back");
    db.conn
        .execute("DROP TRIGGER dbo.write_memo;")
        .await
        .expect("and without the trigger the same insert goes in");
    db.conn.execute(&sql).await.expect("the ordinary case");

    // A rewrite the column's own collation calls equal — the test database
    // is case-insensitive — is still a rewrite: the row is held by the
    // rendering that reads it back, under a binary collation, for a spelled
    // cell and for a defaulted one alike (DECISIONS 137).
    let collation: String = db
        .conn
        .query("SELECT CONVERT(nvarchar(128), DATABASEPROPERTYEX(DB_NAME(), 'Collation'));")
        .await
        .expect("collation")[0]
        .try_get_at::<&str>(0)
        .unwrap()
        .unwrap()
        .to_owned();
    assert!(
        collation.contains("_CI_"),
        "this case needs a case-insensitive database; got {collation}"
    );
    let spelled = pbps_model::Change::InsertRow {
        table: table.clone(),
        key_column: "code".to_owned(),
        identity_key: false,
        key: RowKey::from("sixth"),
        defaults: [("note".to_owned(), "(N'plain')".to_owned())]
            .into_iter()
            .collect(),
        types: [
            ("label".to_owned(), ty("nvarchar(50)")),
            ("note".to_owned(), ty("nvarchar(50)")),
            ("memo".to_owned(), ty("nvarchar(50)")),
        ]
        .into_iter()
        .collect(),
        row: [("label".to_owned(), Value::Text("New".to_owned()))]
            .into_iter()
            .collect::<Row>(),
    };
    let sql = sql_of(&spelled);
    for (trigger, why) in [
        (
            "CREATE TRIGGER dbo.fold_label ON dbo.status AFTER INSERT AS\n\
             BEGIN\n\
               SET NOCOUNT ON;\n\
               UPDATE dbo.status SET label = LOWER(label)\n\
                WHERE code IN (SELECT code FROM inserted);\n\
             END;",
            "a spelled cell rewritten in case alone",
        ),
        (
            "CREATE TRIGGER dbo.fold_note ON dbo.status AFTER INSERT AS\n\
             BEGIN\n\
               SET NOCOUNT ON;\n\
               UPDATE dbo.status SET note = UPPER(note)\n\
                WHERE code IN (SELECT code FROM inserted);\n\
             END;",
            "a defaulted cell rewritten in case alone",
        ),
    ] {
        db.conn.execute(trigger).await.expect(why);
        let err = db
            .conn
            .execute(&sql)
            .await
            .err()
            .unwrap_or_else(|| panic!("{why} was taken for the plan's own:\n{sql}"));
        assert!(
            err.to_string().contains("is not what this plan wrote"),
            "{why}: {err}"
        );
        assert_eq!(
            label_of("sixth").await,
            None,
            "{why}: the insert rolled back"
        );
        let name = trigger.split_whitespace().nth(2).unwrap();
        db.conn
            .execute(&format!("DROP TRIGGER {name};"))
            .await
            .expect("take the folding trigger off");
    }
    db.conn
        .execute(&sql)
        .await
        .expect("and without a trigger the same insert goes in");

    // An update is held to the cells it leaves alone as well, before and
    // after: a trigger rewriting `note`, which the update never sets, and a
    // hand edit to `memo` since the plan was made are both refused
    // (DECISIONS 136).
    db.conn
        .execute("UPDATE dbo.status SET note = N'plain', memo = NULL WHERE code = 'old';")
        .await
        .expect("the row as the plan recorded it");
    let whole_row = pbps_model::Change::UpdateRow {
        table: table.clone(),
        key_column: "code".to_owned(),
        key: RowKey::from("old"),
        columns: [(
            "label".to_owned(),
            (
                Cell::Value(Value::Text("Ancient".to_owned())),
                Cell::Value(Value::Text("Newer".to_owned())),
            ),
        )]
        .into_iter()
        .collect(),
        unchanged: [
            (
                "note".to_owned(),
                Cell::Value(Value::Text("plain".to_owned())),
            ),
            ("memo".to_owned(), Cell::Value(Value::Null)),
        ]
        .into_iter()
        .collect(),
        types: [
            ("label".to_owned(), ty("nvarchar(50)")),
            ("note".to_owned(), ty("nvarchar(50)")),
            ("memo".to_owned(), ty("nvarchar(50)")),
        ]
        .into_iter()
        .collect(),
        after_types: Default::default(),
    };
    let sql = sql_of(&whole_row);
    db.conn
        .execute(
            "CREATE TRIGGER dbo.rewrite_note_on_update ON dbo.status AFTER UPDATE AS\n\
             BEGIN\n\
               SET NOCOUNT ON;\n\
               UPDATE dbo.status SET note = N'rewritten'\n\
                WHERE code IN (SELECT code FROM inserted);\n\
             END;",
        )
        .await
        .expect("a trigger that rewrites a cell the update leaves alone");
    let err = db.conn.execute(&sql).await.err().unwrap_or_else(|| {
        panic!("a rewritten untouched cell was taken for the plan's own:\n{sql}")
    });
    assert!(
        err.to_string().contains("is not what this plan wrote"),
        "{err}"
    );
    assert_eq!(
        label_of("old").await.as_deref(),
        Some("Ancient"),
        "the update rolled back, trigger and all"
    );
    db.conn
        .execute("DROP TRIGGER dbo.rewrite_note_on_update;")
        .await
        .expect("take the rewriting trigger off");
    db.conn
        .execute("UPDATE dbo.status SET memo = N'edited' WHERE code = 'old';")
        .await
        .expect("a hand edit to a cell the plan does not set");
    let err =
        db.conn.execute(&sql).await.err().unwrap_or_else(|| {
            panic!("a hand-edited untouched cell was overwritten around:\n{sql}")
        });
    assert!(
        err.to_string().contains("is not as the plan recorded it"),
        "{err}"
    );
    db.conn
        .execute("UPDATE dbo.status SET memo = NULL WHERE code = 'old';")
        .await
        .expect("the row as recorded again");
    db.conn
        .execute(&sql)
        .await
        .expect("and with the whole row as recorded, the update goes through");
    assert_eq!(label_of("old").await.as_deref(), Some("Newer"));

    // A column this same plan adds and populates has no recorded cell to
    // hold the row to *before* the write — but `AddColumn` has already run by
    // the time the row changes do, so the row is answerable for it after
    // (DECISIONS 140). Held by the base's types alone, this cell was checked
    // by nothing at all and a trigger rewriting it read back as the plan's
    // own result.
    db.conn
        .execute("ALTER TABLE dbo.status ADD tier nvarchar(20) NULL;")
        .await
        .expect("the column this plan adds");
    let added = pbps_model::Change::UpdateRow {
        table: table.clone(),
        key_column: "code".to_owned(),
        key: RowKey::from("old"),
        columns: [
            (
                "label".to_owned(),
                (
                    Cell::Value(Value::Text("Newer".to_owned())),
                    Cell::Value(Value::Text("Newest".to_owned())),
                ),
            ),
            // The differ's `before` for a column the base lacks: NULL by
            // convention, not something the base recorded.
            (
                "tier".to_owned(),
                (
                    Cell::Value(Value::Null),
                    Cell::Value(Value::Text("gold".to_owned())),
                ),
            ),
        ]
        .into_iter()
        .collect(),
        unchanged: Default::default(),
        types: [("label".to_owned(), ty("nvarchar(50)"))]
            .into_iter()
            .collect(),
        after_types: [("tier".to_owned(), ty("nvarchar(20)"))]
            .into_iter()
            .collect(),
    };
    let sql = sql_of(&added);
    db.conn
        .execute(
            "CREATE TRIGGER dbo.rewrite_tier_on_update ON dbo.status AFTER UPDATE AS\n\
             BEGIN\n\
               SET NOCOUNT ON;\n\
               UPDATE dbo.status SET tier = N'bronze'\n\
                WHERE code IN (SELECT code FROM inserted);\n\
             END;",
        )
        .await
        .expect("a trigger that rewrites the column this plan just added");
    let err =
        db.conn.execute(&sql).await.err().unwrap_or_else(|| {
            panic!("a rewritten added cell was taken for the plan's own:\n{sql}")
        });
    assert!(
        err.to_string().contains("is not what this plan wrote"),
        "{err}"
    );
    assert_eq!(
        label_of("old").await.as_deref(),
        Some("Newer"),
        "the update rolled back, trigger and all"
    );
    db.conn
        .execute("DROP TRIGGER dbo.rewrite_tier_on_update;")
        .await
        .expect("take the rewriting trigger off");
    // And nothing holds the added column *before* the write: whatever
    // `AddColumn` left there — here the engine's NULL, but a `NOT NULL` add
    // leaves the default — is not a cell the base recorded.
    db.conn
        .execute("UPDATE dbo.status SET tier = N'silver' WHERE code = 'old';")
        .await
        .expect("something in the column the plan is about to set");
    db.conn
        .execute(&sql)
        .await
        .expect("the added column is not part of the precondition");
    assert_eq!(label_of("old").await.as_deref(), Some("Newest"));

    // And the same for a delete a trigger undoes.
    // `CREATE TRIGGER` must be the first statement of its batch.
    db.conn
        .execute(
            "CREATE TRIGGER dbo.put_back ON dbo.status AFTER DELETE AS\n\
             BEGIN\n\
               SET NOCOUNT ON;\n\
               INSERT INTO dbo.status (code, label) SELECT code, label FROM deleted;\n\
             END;",
        )
        .await
        .expect("a trigger that puts a deleted row back");
    let sql = sql_of(&delete);
    let err = db
        .conn
        .execute(&sql)
        .await
        .err()
        .unwrap_or_else(|| panic!("the delete was taken for this plan's own result:\n{sql}"));
    assert!(
        err.to_string()
            .contains("is back after this plan deleted it"),
        "{err}"
    );
    assert_eq!(
        label_of("old").await.as_deref(),
        Some("Newest"),
        "the row is still there, and still what it was"
    );

    db.drop().await;
}

/// `validate` accepts a schema target it cannot see inside, and this tool
/// never creates a schema — so a grant on one the database does not have is a
/// statement the engine refuses, with everything before it committed under
/// `apply --staged` (DECISIONS 134).
#[tokio::test]
#[ignore = "needs a SQL Server; see scripts/live-tests.sh"]
async fn a_grant_on_a_schema_the_database_does_not_have_is_counted_before_it_runs() {
    let mut db = TestDb::create("grantschema").await;
    db.conn
        .execute("CREATE SCHEMA app;")
        .await
        .expect("one schema that is there");

    let grant = |schema: &str| pbps_model::ChangeSet {
        changes: vec![pbps_model::PlannedChange::new(pbps_model::Change::Grant {
            role: "reader".to_owned(),
            target: pbps_model::GrantTarget::Schema(schema.to_owned()),
            permissions: [pbps_model::Permission::Select].into_iter().collect(),
        })],
    };
    for (schema, want, why) in [
        (
            "legacy",
            1,
            "a schema nobody created is the plan's own mistake",
        ),
        ("app", 0, "and one that is there blocks nothing"),
        // The engine decides what one name is, here as everywhere else — and
        // it also decides how it spells that name. `APP` grants successfully
        // on a case-insensitive database and reads back as `app`, so a plan
        // that wrote it would revoke one spelling and grant the other for
        // ever; the probe counts it (DECISIONS 142).
        (
            "APP",
            1,
            "`APP` is `app` to this database, and `app` is how it would read back",
        ),
    ] {
        let cs = grant(schema);
        let probe = Mssql
            .preflight(&cs)
            .into_iter()
            .find(|p| p.description.contains(schema))
            .unwrap_or_else(|| panic!("the grant on `{schema}` carries a probe"));
        let n: i32 = db
            .conn
            .query(&probe.sql)
            .await
            .unwrap_or_else(|e| panic!("the engine rejected the probe:\n{}\n{e}", probe.sql))[0]
            .try_get_at(0)
            .unwrap()
            .unwrap();
        assert_eq!(n, want, "{why}");
    }

    // A revoke names a securable the same way, and fails on a missing one
    // the same way. An object target is declared, so it exists or this plan
    // creates it: no probe, and none wanted.
    let object = pbps_model::ChangeSet {
        changes: vec![pbps_model::PlannedChange::new(pbps_model::Change::Revoke {
            role: "reader".to_owned(),
            target: pbps_model::GrantTarget::Object(TableName::new("app", "t")),
            permissions: [pbps_model::Permission::Select].into_iter().collect(),
        })],
    };
    assert!(
        Mssql.preflight(&object).is_empty(),
        "an object target carries no probe"
    );

    db.drop().await;
}

/// `money` and `smallmoney` hold four decimal places, and the default
/// conversion style renders two. Read that way, `1.0001` came back `1.00`:
/// `pull` wrote a declaration for a value the table does not hold, and
/// `verify` compared two truncations and called them equal. Measured here
/// rather than reasoned about (DECISIONS 115).
#[tokio::test]
#[ignore = "needs a SQL Server; see scripts/live-tests.sh"]
async fn money_reads_back_with_the_four_decimals_it_holds() {
    use pbps_model::{DataMode, Row, RowKey, RowScope, TableData, Value};

    let text = |s: &str| Value::Text(s.to_owned());
    let mut t = Table::default();
    t.columns
        .insert("id".to_owned(), Column::new(ty("int")).not_null());
    t.columns
        .insert("m".to_owned(), Column::new(ty("money")).not_null());
    t.columns
        .insert("sm".to_owned(), Column::new(ty("smallmoney")).not_null());
    t.primary_key = Some(PrimaryKey {
        name: None,
        columns: vec!["id".to_owned()],
    });
    // The engine's own spelling, which the spelling probe already forces a
    // declaration to use (101): four decimals, always.
    t.data = Some(TableData {
        mode: DataMode::Exact,
        rows: [(
            RowKey::from("1"),
            [
                ("m".to_owned(), text("1.0001")),
                ("sm".to_owned(), text("2.5678")),
            ]
            .into_iter()
            .collect::<Row>(),
        )]
        .into_iter()
        .collect(),
    });
    let name = TableName::new("dbo", "money_t");
    let mut declared = Schema::default();
    declared.tables.insert(name.clone(), t);
    let ids = mint_ids(&declared, &IdsFile::default(), &[]);

    let mut db = TestDb::create("money").await;
    apply(
        &mut db.conn,
        &plan(&Schema::default(), &IdsFile::default(), &declared, &ids),
    )
    .await;

    let scopes = declared.data_scopes();
    let read: std::collections::BTreeMap<TableName, RowScope> = scopes
        .iter()
        .map(|(n, s)| (n.clone(), s.rows_to_read()))
        .collect();
    let live = pbps_mssql::catalog::introspect(&mut db.conn)
        .await
        .expect("introspect")
        .schema;
    let observed = pbps_mssql::catalog::read_rows(&mut db.conn, &live, &read)
        .await
        .expect("read rows");
    let back = live
        .with_observed_rows(&observed, &scopes, &declared)
        .unwrap();
    let row = &back.tables[&name].data.as_ref().unwrap().rows[&RowKey::from("1")];
    let cell = |c: &str| {
        row.columns()
            .find(|(n, _)| n.as_str() == c)
            .map(|(_, v)| v.clone())
    };
    // The fourth decimal survives. Under the default style these were
    // `1.00` and `2.57`, and the declaration above would have been silently
    // rewritten to them.
    assert_eq!(cell("m"), Some(text("1.0001")), "{row:?}");
    assert_eq!(cell("sm"), Some(text("2.5678")), "{row:?}");
    // Which is the property that matters: the read agrees with the
    // declaration, so the drift check has nothing to say.
    assert_eq!(back.tables[&name].data, declared.tables[&name].data);

    db.drop().await;
}

/// The catalog cannot tell `label: Unlabelled` from an omitted `label`, so
/// each side reads the cell in its own spelling — and a default the engine
/// would have to run (`NEXT VALUE FOR`, `SYSUTCDATETIME()`) is never put in
/// the read: the first is not even legal in a `CASE`, and both would run once
/// per row of a drift check. Measured here rather than reasoned about.
#[tokio::test]
#[ignore = "needs a SQL Server; see scripts/live-tests.sh"]
async fn an_explicit_default_stays_explicit_and_a_volatile_default_is_never_run() {
    use pbps_model::{DataMode, Row, RowKey, RowScope, TableData, Value};

    let text = |s: &str| Value::Text(s.to_owned());
    let mut t = Table::default();
    t.columns
        .insert("id".to_owned(), Column::new(ty("int")).not_null());
    let mut label = Column::new(ty("nvarchar(50)")).not_null();
    label.default = Some("'Unlabelled'".to_owned());
    t.columns.insert("label".to_owned(), label);
    let mut seq = Column::new(ty("int")).not_null();
    // In the engine's spelling, as every default has to be (§8.2): the
    // catalog stores `NEXT VALUE FOR [dbo].[seq]` whatever was written.
    seq.default = Some("NEXT VALUE FOR [dbo].[seq]".to_owned());
    t.columns.insert("seq".to_owned(), seq);
    let mut stamp = Column::new(ty("datetime2")).not_null();
    stamp.default = Some("sysutcdatetime()".to_owned());
    t.columns.insert("stamp".to_owned(), stamp);
    t.primary_key = Some(PrimaryKey {
        name: Some("pk_t".to_owned()),
        columns: vec!["id".to_owned()],
    });
    let rows = |cells: &[(&str, &[(&str, Value)])]| -> std::collections::BTreeMap<RowKey, Row> {
        cells
            .iter()
            .map(|(k, cs)| {
                (
                    RowKey::from(*k),
                    cs.iter()
                        .map(|(c, v)| ((*c).to_owned(), v.clone()))
                        .collect::<Row>(),
                )
            })
            .collect()
    };
    t.data = Some(TableData {
        mode: DataMode::Exact,
        rows: rows(&[
            // Written explicitly, and equal to the default.
            ("1", &[("label", text("Unlabelled"))]),
            // Omitted: the default, whatever it is.
            ("2", &[]),
        ]),
    });
    let name = TableName::new("dbo", "t");
    let mut declared = Schema::default();
    declared.tables.insert(name.clone(), t);
    let ids = mint_ids(&declared, &IdsFile::default(), &[]);

    let mut db = TestDb::create("voldefault").await;
    db.conn
        .execute("CREATE SEQUENCE dbo.seq START WITH 100 INCREMENT BY 1;")
        .await
        .expect("sequence");
    apply(
        &mut db.conn,
        &plan(&Schema::default(), &IdsFile::default(), &declared, &ids),
    )
    .await;

    let scopes = declared.data_scopes();
    let read: std::collections::BTreeMap<TableName, RowScope> = scopes
        .iter()
        .map(|(n, s)| (n.clone(), s.rows_to_read()))
        .collect();
    let live = pbps_mssql::catalog::introspect(&mut db.conn)
        .await
        .expect("introspect")
        .schema;
    let observed = pbps_mssql::catalog::read_rows(&mut db.conn, &live, &read)
        .await
        .expect("a table with a NEXT VALUE FOR default reads back");

    // The sequence was consumed by the two inserts and by nothing else: a
    // read that evaluated the default would have advanced it further.
    let next: i32 = db
        .conn
        .query("SELECT CAST(current_value AS int) AS v FROM sys.sequences WHERE name = 'seq';")
        .await
        .expect("sequence state")[0]
        .try_get("v")
        .expect("v")
        .expect("v");
    assert_eq!(next, 101, "two inserts, no evaluation in the read");

    // As the declarations see it: what was declared is what comes back,
    // explicit `label` included, and the two volatile cells omitted.
    let as_declared = live
        .clone()
        .with_observed_rows(&observed, &scopes, &declared)
        .unwrap();
    assert_eq!(as_declared.tables[&name].data, declared.tables[&name].data);
    // As `pull` sees it, with no rows of its own: the label, which the
    // engine confirmed at its default, is omitted; the sequence value and
    // the stamp, whose defaults were never asked about, are kept — a
    // generated value is a value the block has to carry.
    let pulled = live
        .with_observed_rows(&observed, &scopes, &Schema::default())
        .unwrap();
    for (key, row) in &pulled.tables[&name].data.as_ref().unwrap().rows {
        let columns: Vec<&String> = row.columns().map(|(c, _)| c).collect();
        assert_eq!(columns, ["seq", "stamp"], "row {key}");
    }
    // And the differ, measured against the declared reading, has nothing to
    // say about the rows — which is the property the whole thing exists for.
    let cs = pbps_diff::diff_partial(
        pbps_diff::Side {
            schema: &as_declared,
            ids: &ids,
        },
        pbps_diff::Side {
            schema: &declared,
            ids: &ids,
        },
        &Mssql,
        &pbps_model::Hints::default(),
    );
    assert!(cs.errors.is_empty(), "{:?}", cs.errors);
    let rows_changed: Vec<String> = cs
        .changes
        .changes
        .iter()
        .filter(|p| p.change.table() == Some(&name))
        .map(|p| format!("{:?}", p.change))
        .collect();
    assert_eq!(rows_changed, Vec::<String>::new());

    db.drop().await;
}

/// The engine spells an `int` key `1`; the declaration wrote `01`. Whether
/// they name the same row is the engine's call (§8.2), and it has to be the
/// same call the DML makes with `WHERE [id] = N'01'` — so the read asks it,
/// and each side gets its rows back under its own spelling. Before this, an
/// `exact` block of `01` planned an insert of `01` and a delete of `1` on
/// every connected plan, and an `ensure` block could not find its row.
#[tokio::test]
#[ignore = "needs a SQL Server; see scripts/live-tests.sh"]
async fn a_declared_key_keeps_its_spelling_when_the_engine_spells_it_differently() {
    use pbps_model::{DataMode, Row, RowKey, RowScope, TableData, Value};

    let text = |s: &str| Value::Text(s.to_owned());
    let table_with = |mode: DataMode, keys: &[&str]| {
        let mut t = Table::default();
        t.columns
            .insert("id".to_owned(), Column::new(ty("int")).not_null());
        t.columns.insert(
            "label".to_owned(),
            Column::new(ty("nvarchar(50)")).not_null(),
        );
        t.primary_key = Some(PrimaryKey {
            name: Some("pk_k".to_owned()),
            columns: vec!["id".to_owned()],
        });
        t.data = Some(TableData {
            mode,
            rows: keys
                .iter()
                .map(|k| {
                    (
                        RowKey::from(*k),
                        [("label".to_owned(), text(&format!("row {k}")))]
                            .into_iter()
                            .collect::<Row>(),
                    )
                })
                .collect(),
        });
        t
    };
    let name = TableName::new("dbo", "k");
    let mut declared = Schema::default();
    declared
        .tables
        .insert(name.clone(), table_with(DataMode::Exact, &["01", "7"]));
    let ids = mint_ids(&declared, &IdsFile::default(), &[]);

    let mut db = TestDb::create("keyspelling").await;
    apply(
        &mut db.conn,
        &plan(&Schema::default(), &IdsFile::default(), &declared, &ids),
    )
    .await;

    let read_under = |schema: &Schema| {
        let scopes = schema.data_scopes();
        let read: std::collections::BTreeMap<TableName, RowScope> = scopes
            .iter()
            .map(|(n, s)| (n.clone(), s.rows_to_read()))
            .collect();
        (scopes, read)
    };
    let (scopes, read) = read_under(&declared);
    let live = pbps_mssql::catalog::introspect(&mut db.conn)
        .await
        .expect("introspect")
        .schema;
    let observed = pbps_mssql::catalog::read_rows(&mut db.conn, &live, &read)
        .await
        .expect("read rows");
    // The engine's spelling, and the alias the read asked about.
    let seen = &observed[&name];
    let keys: Vec<String> = seen.rows.keys().map(ToString::to_string).collect();
    assert_eq!(keys, ["1", "7"]);
    assert_eq!(seen.aliases[&RowKey::from("01")], RowKey::from("1"));

    // As declared: the declaration's own spelling, and nothing to plan.
    let as_declared = live
        .clone()
        .with_observed_rows(&observed, &scopes, &declared)
        .unwrap();
    assert_eq!(as_declared.tables[&name].data, declared.tables[&name].data);
    let cs = pbps_diff::diff_partial(
        pbps_diff::Side {
            schema: &as_declared,
            ids: &ids,
        },
        pbps_diff::Side {
            schema: &declared,
            ids: &ids,
        },
        &Mssql,
        &pbps_model::Hints::default(),
    );
    assert!(cs.errors.is_empty(), "{:?}", cs.errors);
    let rows_changed: Vec<String> = cs
        .changes
        .changes
        .iter()
        .filter(|p| p.change.table() == Some(&name))
        .map(|p| format!("{:?}", p.change))
        .collect();
    assert_eq!(rows_changed, Vec::<String>::new());

    // As `pull` sees it — a scope that spells no key — the engine's spelling.
    let unspelled: pbps_model::DataScopes = scopes
        .iter()
        .map(|(n, s)| {
            (
                n.clone(),
                pbps_model::DataScope {
                    mode: s.mode,
                    keys: Default::default(),
                },
            )
        })
        .collect();
    let pulled = live
        .clone()
        .with_observed_rows(&observed, &unspelled, &Schema::default())
        .unwrap();
    let keys: Vec<String> = pulled.tables[&name]
        .data
        .as_ref()
        .unwrap()
        .rows
        .keys()
        .map(ToString::to_string)
        .collect();
    assert_eq!(keys, ["1", "7"]);

    // An `ensure` block that names `01` finds its row through the alias.
    let mut ensured = Schema::default();
    ensured
        .tables
        .insert(name.clone(), table_with(DataMode::Ensure, &["01"]));
    let (scopes, read) = read_under(&ensured);
    let observed = pbps_mssql::catalog::read_rows(&mut db.conn, &live, &read)
        .await
        .expect("read rows");
    let as_ensured = live
        .with_observed_rows(&observed, &scopes, &ensured)
        .unwrap();
    assert_eq!(as_ensured.tables[&name].data, ensured.tables[&name].data);

    db.drop().await;
}

/// Roles against the engine (ADR-0005): the plan's `CREATE ROLE` and `GRANT`
/// are accepted, the catalog reads the role and its grants back exactly, a
/// hand-made `GRANT` and a `DENY` are seen for what they are, and a rename
/// goes through `ALTER ROLE ... WITH NAME` with the membership — the one thing
/// the declarations cannot restore — still attached afterwards.
#[tokio::test]
#[ignore = "needs a live SQL Server; run scripts/live-tests.sh"]
async fn roles_and_grants_round_trip_and_a_rename_keeps_the_members() {
    use pbps_model::{GrantTarget, Permission, Role};

    let mut customer = Table::default();
    customer
        .columns
        .insert("id".to_owned(), Column::new(ty("int")).not_null());
    customer.primary_key = Some(PrimaryKey {
        name: Some("pk_customer".to_owned()),
        columns: vec!["id".to_owned()],
    });
    let mut reader = Role::default();
    reader.grants.insert(
        GrantTarget::Object(TableName::new("dbo", "customer")),
        [Permission::Select, Permission::ViewDefinition]
            .into_iter()
            .collect(),
    );
    reader.grants.insert(
        GrantTarget::Schema("dbo".to_owned()),
        [Permission::Execute].into_iter().collect(),
    );
    let mut declared = Schema::default();
    declared
        .tables
        .insert(TableName::new("dbo", "customer"), customer);
    declared
        .roles
        .insert("app_reader".to_owned(), reader.clone());
    let ids = mint_ids(&declared, &IdsFile::default(), &[]);
    assert_eq!(ids.roles.len(), 1);

    let mut db = TestDb::create("roles").await;
    apply(
        &mut db.conn,
        &plan(&Schema::default(), &IdsFile::default(), &declared, &ids),
    )
    .await;

    let pulled = pbps_mssql::catalog::introspect(&mut db.conn)
        .await
        .expect("introspect");
    assert_eq!(pulled.warnings, Vec::<String>::new());
    assert_eq!(pulled.schema.roles.get("app_reader"), Some(&reader));
    // Scoped by the ids file, the live state is the declaration.
    let scoped = pbps_diff::scope(&pulled.schema, &ids, &Default::default());
    assert_eq!(scoped.schema.roles, declared.roles);
    assert!(
        scoped.unmanaged_roles.is_empty(),
        "{:?}",
        scoped.unmanaged_roles
    );

    // Hand edits: a grant nobody declared, and a DENY, which is not modelled
    // and must be reported rather than folded into anything.
    db.conn
        .execute(
            "GRANT INSERT ON OBJECT::dbo.customer TO app_reader;\n\
             DENY DELETE ON OBJECT::dbo.customer TO app_reader;\n\
             GRANT UPDATE ON OBJECT::dbo.customer TO app_reader WITH GRANT OPTION;\n\
             GRANT CREATE TABLE TO app_reader;",
        )
        .await
        .expect("hand edits");
    let again = pbps_mssql::catalog::introspect(&mut db.conn)
        .await
        .expect("introspect");
    // A database-level grant is class 0 in the catalog, which the read once
    // filtered out before anything could report it; measured here as the
    // engine records it (DECISIONS 105).
    assert!(
        again
            .unexpressible
            .iter()
            .any(|u| u.role == "app_reader" && u.what.contains("CREATE TABLE on the database")),
        "{:?}",
        again.unexpressible
    );
    // The DENY is not a warning: a managed role that gained one has changed,
    // and the sets that remain would compare equal (DECISIONS 97).
    assert!(
        again
            .unexpressible
            .iter()
            .any(|u| u.role == "app_reader" && u.what.contains("DENY DELETE")),
        "{:?}",
        again.unexpressible
    );
    // WITH GRANT OPTION is wider than any grant a declaration can spell: it
    // is not folded into the set (that read a widened role as clean) and it
    // is reported as unexpressible, where a drift check has to see it
    // (DECISIONS 95). The catalog spells the state `W`; measured here.
    assert!(
        again.unexpressible.iter().any(|u| u.role == "app_reader"
            && u.what.contains("UPDATE")
            && u.what.contains("WITH GRANT OPTION")),
        "{:?}",
        again.unexpressible
    );
    assert!(
        !again.schema.roles["app_reader"]
            .grants
            .get(&"dbo.customer".parse().unwrap())
            .is_some_and(|g| g.contains(&Permission::Update)),
        "the widened grant must not read as the plain one"
    );
    let live = pbps_diff::scope(&again.schema, &ids, &Default::default()).schema;
    let drift = pbps_diff::diff_partial(
        pbps_diff::Side {
            schema: &scoped.schema,
            ids: &ids,
        },
        pbps_diff::Side {
            schema: &live,
            ids: &pbps_diff::observed_ids(&live, &ids),
        },
        &Mssql,
        &pbps_model::Hints::default(),
    );
    assert!(drift.errors.is_empty(), "{:?}", drift.errors);
    let described: Vec<String> = drift
        .changes
        .changes
        .iter()
        .map(|p| format!("{:?}", p.change))
        .collect();
    assert_eq!(described.len(), 1, "{described:?}");
    assert!(
        described[0].starts_with("Grant") && described[0].contains("Insert"),
        "{described:?}"
    );

    // Membership is the environment's own. A user without a login is enough
    // to hold it, and it has to survive the rename.
    db.conn
        .execute(
            "CREATE USER pbps_member WITHOUT LOGIN;\n\
             ALTER ROLE app_reader ADD MEMBER pbps_member;",
        )
        .await
        .expect("a member");
    let mut renamed = declared.clone();
    let role = renamed.roles.remove("app_reader").unwrap();
    renamed.roles.insert("reader".to_owned(), role);
    let renamed_ids = mint_ids(
        &renamed,
        &ids,
        &[Intent::RenameRole {
            from: "app_reader".to_owned(),
            to: "reader".to_owned(),
        }],
    );
    assert_eq!(renamed_ids.roles, {
        let mut r = ids.roles.clone();
        for v in r.values_mut() {
            *v = "reader".to_owned();
        }
        r
    });
    let cs = plan(&scoped.schema, &ids, &renamed, &renamed_ids);
    let statements: Vec<String> = cs
        .changes
        .iter()
        .flat_map(|p| Mssql.emit(&p.change, p.strategy).expect("emit"))
        .map(|s| s.sql)
        .collect();
    assert_eq!(
        statements,
        ["ALTER ROLE [app_reader] WITH NAME = [reader];"],
        "a rename is one ALTER, never a drop"
    );
    apply(&mut db.conn, &cs).await;
    let members = db
        .conn
        .query(
            "SELECT COUNT(*) FROM sys.database_role_members rm \
             JOIN sys.database_principals r ON r.principal_id = rm.role_principal_id \
             JOIN sys.database_principals m ON m.principal_id = rm.member_principal_id \
             WHERE r.name = 'reader' AND m.name = 'pbps_member';",
        )
        .await
        .expect("members");
    let n: i32 = members[0].try_get_at(0).unwrap().unwrap();
    assert_eq!(n, 1, "the member must still hold the renamed role");

    // Dropping the role while it has a member: the connected plan lists the
    // member and removes it first, and the engine then accepts the drop.
    let found = pbps_mssql::catalog::role_members(&mut db.conn)
        .await
        .expect("members");
    assert_eq!(found.get("reader"), Some(&vec!["pbps_member".to_owned()]));
    let drop = pbps_model::ChangeSet {
        changes: vec![pbps_model::PlannedChange::new(
            pbps_model::Change::DropRole {
                uid: renamed_ids.roles.keys().next().unwrap().clone(),
                name: "reader".to_owned(),
                members: found["reader"].clone(),
            },
        )],
    };
    apply(&mut db.conn, &drop).await;
    let left = db
        .conn
        .query("SELECT COUNT(*) FROM sys.database_principals WHERE name = 'reader';")
        .await
        .expect("query");
    let n: i32 = left[0].try_get_at(0).unwrap().unwrap();
    assert_eq!(n, 0, "the role is gone");
    // The negative case, against the engine: without the members listed the
    // drop is refused, which is the failure the listing exists to avoid.
    db.conn
        .execute("CREATE ROLE stuck; ALTER ROLE stuck ADD MEMBER pbps_member;")
        .await
        .expect("a role with a member");
    assert!(
        db.conn.execute("DROP ROLE stuck;").await.is_err(),
        "the engine must refuse to drop a role that has members"
    );

    // The permission rule `validate` applies is the engine's, measured here
    // on the pair the rule exists for (DECISIONS 89): a table is not
    // executed, and the engine says so with Msg 4606 rather than by ignoring
    // it — a staged apply would have committed everything before this.
    db.conn
        .execute("CREATE ROLE perm_probe;")
        .await
        .expect("a role to grant to");
    let refused = db
        .conn
        .execute("GRANT EXECUTE ON dbo.customer TO perm_probe;")
        .await;
    assert!(
        refused.is_err(),
        "the engine must refuse EXECUTE on a table: {refused:?}"
    );
    db.conn
        .execute("GRANT SELECT ON dbo.customer TO perm_probe; DROP ROLE perm_probe;")
        .await
        .expect("the permission a table does take");

    // Ownership is the other thing the engine refuses to drop a role over,
    // and the plan has to see it before anything runs.
    db.conn
        .execute("CREATE ROLE owner_role; EXEC('CREATE SCHEMA owned AUTHORIZATION owner_role');")
        .await
        .expect("a role that owns a schema");
    let owned = pbps_mssql::catalog::role_owned_securables(&mut db.conn)
        .await
        .expect("owned securables");
    assert_eq!(
        owned.get("owner_role"),
        Some(&vec!["SCHEMA::owned".to_owned()])
    );
    assert_eq!(
        owned.get("stuck"),
        None,
        "owning nothing is absent, not empty"
    );
    assert!(
        db.conn.execute("DROP ROLE owner_role;").await.is_err(),
        "the engine must refuse to drop a role that owns a schema"
    );
    // A schema is one class of many. The first list named the ones that
    // came to mind and missed a role that owns another role; the check now
    // covers every class the catalog records an owner for (DECISIONS 88),
    // and three of them are measured here — the one that was missed, a
    // Service Broker object, and a schema-scoped one whose owner is only
    // recorded when it differs from the schema's.
    db.conn
        .execute(
            "CREATE ROLE owned_role AUTHORIZATION owner_role; \
             CREATE MESSAGE TYPE owned_mt AUTHORIZATION owner_role VALIDATION = NONE; \
             CREATE XML SCHEMA COLLECTION owned_xsc AS \
             N'<schema xmlns=\"http://www.w3.org/2001/XMLSchema\" targetNamespace=\"urn:x\">\
             <element name=\"r\" type=\"string\"/></schema>'; \
             ALTER AUTHORIZATION ON XML SCHEMA COLLECTION::dbo.owned_xsc TO owner_role;",
        )
        .await
        .expect("a role that owns a role, a message type and an XML schema collection");
    let owned = pbps_mssql::catalog::role_owned_securables(&mut db.conn)
        .await
        .expect("owned securables");
    assert_eq!(
        owned.get("owner_role"),
        Some(&vec![
            "MESSAGE TYPE::owned_mt".to_owned(),
            "ROLE::owned_role".to_owned(),
            "SCHEMA::owned".to_owned(),
            "XML SCHEMA COLLECTION::dbo.owned_xsc".to_owned(),
        ])
    );
    // The negative case, against the engine: with everything but the role
    // handed back, owning a role alone still blocks the drop — and handing
    // that back too is what lets it through, so the list was the whole
    // reason.
    db.conn
        .execute(
            "ALTER AUTHORIZATION ON SCHEMA::owned TO dbo; \
             ALTER AUTHORIZATION ON MESSAGE TYPE::owned_mt TO dbo; \
             ALTER AUTHORIZATION ON XML SCHEMA COLLECTION::dbo.owned_xsc TO dbo;",
        )
        .await
        .expect("ownership moved back");
    assert!(
        db.conn.execute("DROP ROLE owner_role;").await.is_err(),
        "the engine must refuse to drop a role that owns another role"
    );
    db.conn
        .execute("ALTER AUTHORIZATION ON ROLE::owned_role TO dbo; DROP ROLE owner_role;")
        .await
        .expect("a role that owns nothing can be dropped");

    db.drop().await;
}

/// `doctor` and roles (ADR-0005): a least-privilege login that can deploy
/// tables is not ready to deploy a role — and the gaps are reported where the
/// grants have to go, at the database for the role itself and on the securable
/// for the `GRANT`. Only a real `HAS_PERMS_BY_NAME` can say whether `CONTROL`
/// at object scope is answered the way this code expects.
#[tokio::test]
#[ignore = "needs a live SQL Server; run scripts/live-tests.sh"]
async fn the_readiness_check_asks_for_role_permissions_only_where_a_role_is_granted() {
    let mut db = TestDb::create("doctorrole").await;
    let login = format!("pbps_lr_{}", std::process::id());
    let password = "pbpsLeastPrivilege!1";
    db.conn
        .execute(&format!(
            "USE master; \
             IF SUSER_ID('{login}') IS NOT NULL DROP LOGIN [{login}]; \
             CREATE LOGIN [{login}] WITH PASSWORD = '{password}', CHECK_POLICY = OFF;"
        ))
        .await
        .expect("create login");
    db.conn
        .execute(&format!(
            "USE [{0}]; \
             CREATE TABLE dbo.customer (id int NOT NULL PRIMARY KEY); \
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
    let mut lp = connect_live(&as_login).await.expect("connect as the login");

    // The control: with no role declared, this account is ready.
    let held = pbps_mssql::doctor::permissions(
        &mut lp,
        &["dbo".to_owned()],
        &[],
        &pbps_mssql::doctor::GrantTargets::default(),
        &pbps_mssql::doctor::DataTables::new(),
    )
    .await
    .expect("read permissions");
    assert!(
        pbps_mssql::doctor::missing(&held).is_empty(),
        "{:?}",
        pbps_mssql::doctor::missing(&held)
    );

    // With a role granted on the table and on the schema: three gaps, each
    // where the grant has to go.
    let targets = pbps_mssql::doctor::GrantTargets {
        objects: vec!["dbo.customer".parse().unwrap()],
        schemas: vec!["dbo".to_owned()],
        roles: vec![],
    };
    let held = pbps_mssql::doctor::permissions(
        &mut lp,
        &["dbo".to_owned()],
        &[],
        &targets,
        &pbps_mssql::doctor::DataTables::new(),
    )
    .await
    .expect("read permissions");
    let gaps = pbps_mssql::doctor::missing(&held);
    let mut where_missing: Vec<String> = gaps
        .iter()
        .map(|g| format!("{} on {}", g.permission, g.securable()))
        .collect();
    where_missing.sort();
    assert_eq!(
        where_missing,
        [
            "ALTER ANY ROLE on the database",
            "CONTROL on OBJECT::[dbo].[customer]",
            "CONTROL on SCHEMA::[dbo]",
            "CREATE ROLE on the database",
        ],
        "{gaps:?}"
    );

    // A managed role that holds a grant the declarations no longer name: the
    // next plan revokes it, so the securable it is on is asked about too —
    // in another schema, so the schema-scoped CONTROL below does not cover
    // it by accident.
    // `CREATE SCHEMA` has to be a batch of its own.
    db.conn
        .execute(&format!(
            "USE [{0}]; EXEC('CREATE SCHEMA legacy');",
            db.name
        ))
        .await
        .expect("a schema outside the declarations");
    db.conn
        .execute(&format!(
            "USE [{0}]; \
             CREATE TABLE legacy.archive (id int NOT NULL PRIMARY KEY); \
             CREATE ROLE app_reader; \
             GRANT SELECT ON legacy.archive TO app_reader; \
             GRANT SELECT ON SCHEMA::legacy TO app_reader;",
            db.name
        ))
        .await
        .expect("a role with a grant outside the declarations");
    // The login cannot see `app_reader`'s grants in the catalog — metadata
    // visibility hides a securable from an account with no permission on it,
    // which is exactly the account being checked — so the recorded state is
    // where the check has to read them from.
    let mut recorded = Schema::default();
    let mut app_reader = pbps_model::Role::default();
    app_reader.grants.insert(
        pbps_model::GrantTarget::Object(TableName::new("legacy", "archive")),
        [pbps_model::Permission::Select].into_iter().collect(),
    );
    app_reader.grants.insert(
        pbps_model::GrantTarget::Schema("legacy".to_owned()),
        [pbps_model::Permission::Select].into_iter().collect(),
    );
    recorded.roles.insert("app_reader".to_owned(), app_reader);
    pbps_mssql::state::ensure_tables(&mut db.conn)
        .await
        .expect("ledger");
    pbps_mssql::state::record(
        &mut db.conn,
        &StateSnapshot::new(
            pbps_model::StateKind::Apply,
            recorded,
            IdsFile::default(),
            "live-test",
        ),
    )
    .await
    .expect("record the state with the role");
    let managed = pbps_mssql::doctor::GrantTargets {
        roles: vec!["app_reader".to_owned()],
        ..targets.clone()
    };
    let held = pbps_mssql::doctor::permissions(
        &mut lp,
        &["dbo".to_owned()],
        &[],
        &managed,
        &pbps_mssql::doctor::DataTables::new(),
    )
    .await
    .expect("read permissions");
    let gaps = pbps_mssql::doctor::missing(&held);
    let mut where_missing: Vec<String> = gaps
        .iter()
        .map(|g| format!("{} on {}", g.permission, g.securable()))
        .collect();
    where_missing.sort();
    assert_eq!(
        where_missing,
        [
            "ALTER ANY ROLE on the database",
            "CONTROL on OBJECT::[dbo].[customer]",
            "CONTROL on OBJECT::[legacy].[archive]",
            "CONTROL on SCHEMA::[dbo]",
            "CONTROL on SCHEMA::[legacy]",
            "CREATE ROLE on the database",
        ],
        "{gaps:?}"
    );

    // A schema a role is granted on that the database does not have is
    // reported as absent, not silently unasked: the GRANT would fail.
    let nowhere = pbps_mssql::doctor::GrantTargets {
        schemas: vec!["dbo".to_owned(), "nowhere".to_owned()],
        ..targets.clone()
    };
    let held = pbps_mssql::doctor::permissions(
        &mut lp,
        &["dbo".to_owned()],
        &[],
        &nowhere,
        &pbps_mssql::doctor::DataTables::new(),
    )
    .await
    .expect("read permissions");
    assert_eq!(
        held.absent_schemas.iter().cloned().collect::<Vec<_>>(),
        ["nowhere"]
    );

    // Granted exactly what the gaps name, the account is ready — and CONTROL
    // on the schema covers the object inside it, which is the inheritance
    // `HAS_PERMS_BY_NAME` has to account for.
    db.conn
        .execute(&format!(
            "USE [{0}]; \
             GRANT CREATE ROLE, ALTER ANY ROLE TO [{login}]; \
             GRANT CONTROL ON SCHEMA::dbo TO [{login}]; \
             GRANT CONTROL ON SCHEMA::legacy TO [{login}];",
            db.name
        ))
        .await
        .expect("grant the role permissions");
    let held = pbps_mssql::doctor::permissions(
        &mut lp,
        &["dbo".to_owned()],
        &[],
        &managed,
        &pbps_mssql::doctor::DataTables::new(),
    )
    .await
    .expect("read permissions");
    assert!(
        pbps_mssql::doctor::missing(&held).is_empty(),
        "{:?}",
        pbps_mssql::doctor::missing(&held)
    );

    drop(lp);
    db.drop().await;
    let mut master = connect_live(&conn_str()).await.expect("connect");
    let _ = master
        .execute(&format!("USE master; DROP LOGIN [{login}];"))
        .await;
}

/// A revision that retypes a column *and* changes one of its declared rows
/// still holds that row to what the plan recorded (DECISIONS 149).
///
/// Only a live server can answer this. `AlterColumnType` sorts before every
/// row change, so by the time the `UPDATE` or `DELETE` runs the column holds
/// the value the engine converted, and the recorded text is the old type's
/// spelling of it. 146 read that as "neither type can compare it" and dropped
/// the predicate — which dropped the stale-row guard on exactly the cell a
/// concurrent session is most likely to have moved. The predicate now asks the
/// engine to run the same conversion on the recorded text, and what has to be
/// measured is that the answer matches for an untouched row and does not for
/// an edited one, across the renderings that carry a style.
#[tokio::test]
#[ignore = "needs a live SQL Server; run scripts/live-tests.sh"]
async fn a_row_write_across_a_retyped_column_still_holds_the_recorded_row() {
    use pbps_model::{DataMode, Row, RowKey, TableData, Value};

    fn rows(rows: &[(&str, &[(&str, Value)])]) -> std::collections::BTreeMap<RowKey, Row> {
        rows.iter()
            .map(|(k, cells)| {
                (
                    RowKey::from(*k),
                    cells
                        .iter()
                        .map(|(c, v)| ((*c).to_owned(), v.clone()))
                        .collect::<Row>(),
                )
            })
            .collect()
    }
    let text = |s: &str| Value::Text(s.to_owned());

    // `pct` is `decimal(5,2)` and `since` a `varchar(10)`: two renderings that
    // need a style to come back, and two conversions that lose something.
    let table = |pct: &str, since: &str, rs: &[(&str, &[(&str, Value)])]| {
        let mut t = Table::default();
        t.columns
            .insert("code".to_owned(), Column::new(ty("varchar(20)")).not_null());
        t.columns.insert("pct".to_owned(), Column::new(ty(pct)));
        t.columns.insert("since".to_owned(), Column::new(ty(since)));
        t.columns
            .insert("note".to_owned(), Column::new(ty("nvarchar(50)")));
        t.primary_key = Some(PrimaryKey {
            name: None,
            columns: vec!["code".to_owned()],
        });
        t.data = Some(TableData {
            mode: DataMode::Exact,
            rows: rows(rs),
        });
        let mut s = Schema::default();
        s.tables.insert(TableName::new("dbo", "t"), t);
        s
    };

    let first = table(
        "decimal(5,2)",
        "varchar(10)",
        &[
            (
                "keep",
                &[
                    ("pct", text("1.50")),
                    ("since", text("2026-09-03")),
                    ("note", text("first")),
                ],
            ),
            (
                "drop",
                &[
                    ("pct", text("2.75")),
                    ("since", text("2026-09-01")),
                    ("note", text("doomed")),
                ],
            ),
        ],
    );
    let ids = mint_ids(&first, &IdsFile::default(), &[]);

    let mut db = TestDb::create("retyped_row").await;
    apply(
        &mut db.conn,
        &plan(&Schema::default(), &IdsFile::default(), &first, &ids),
    )
    .await;

    // The second revision does all three at once: retype both columns, change
    // `keep`'s note, and stop declaring `drop`.
    let second = table(
        "int",
        "date",
        &[(
            "keep",
            &[
                ("pct", text("1")),
                ("since", text("2026-09-03")),
                ("note", text("second")),
            ],
        )],
    );
    let second_ids = mint_ids(&second, &ids, &[]);
    let changes = plan(&first, &ids, &second, &second_ids);

    async fn one(conn: &mut Conn, sql: &str) -> i32 {
        let rows = conn
            .query(sql)
            .await
            .unwrap_or_else(|e| panic!("the engine rejected:\n{sql}\n{e}"));
        rows[0].try_get_at(0).unwrap().unwrap()
    }

    // A cell another session moves after the plan was read, on the column the
    // plan retypes, in the row the plan updates and in the row it deletes.
    // Under 146 both went through: the `ALTER` converted the new value and the
    // row change overwrote or removed it with nothing saying so.
    for (key, column, edit) in [
        ("keep", "pct", "pct = 9.25"),
        ("keep", "since", "since = '2026-01-01'"),
        ("drop", "pct", "pct = 9.25"),
        ("drop", "since", "since = '2026-01-01'"),
    ] {
        db.conn
            .execute(&format!("UPDATE dbo.t SET {edit} WHERE code = '{key}';"))
            .await
            .expect("a hand edit after the plan");
        let err = try_apply(&mut db.conn, &changes)
            .await
            .expect_err("the row is not as the plan recorded it");
        assert!(
            err.contains(&format!("dbo.t row `{key}` is not as the plan recorded it")),
            "a {column} edit under a retyped column was not seen:\n{err}"
        );
        assert_eq!(
            one(
                &mut db.conn,
                "SELECT COUNT(*) FROM dbo.t WHERE code = 'drop';"
            )
            .await,
            1,
            "nothing of the plan stays after the refusal"
        );
        // Put it back the way the plan recorded it, in the type the column
        // still has, so the next case starts from the reviewed baseline.
        db.conn
            .execute(
                "UPDATE dbo.t SET pct = 1.50, since = '2026-09-03' WHERE code = 'keep';\n\
                 UPDATE dbo.t SET pct = 2.75, since = '2026-09-01' WHERE code = 'drop';",
            )
            .await
            .expect("the edits undone");
    }

    // And with the rows as the plan recorded them, the same plan goes through:
    // the conversion the predicate asks for is the one the `ALTER` ran, so an
    // untouched row still matches after it.
    try_apply(&mut db.conn, &changes)
        .await
        .expect("the rows are as the plan recorded them");
    assert_eq!(
        one(
            &mut db.conn,
            "SELECT COUNT(*) FROM dbo.t WHERE code = 'keep' AND pct = 1 \
             AND since = '2026-09-03' AND note = N'second';"
        )
        .await,
        1,
        "the update must have run against the converted row"
    );
    assert_eq!(
        one(
            &mut db.conn,
            "SELECT COUNT(*) FROM dbo.t WHERE code = 'drop';"
        )
        .await,
        0,
        "the undeclared row must have been deleted"
    );

    db.drop().await;
}

/// A plan that repairs the data and *then* adds the foreign key is a plan the
/// engine accepts, and the probe must agree with it (DECISIONS 151).
///
/// The probe reads the tables before a statement has run, and
/// `AddForeignKey` sorts after every row change — so a probe built from what
/// is stored answers about a state that will not exist. Only a live server can
/// settle both halves at once: that the query the probe now writes runs, and
/// that its count is the number of rows the `ALTER TABLE ... ADD CONSTRAINT`
/// actually refuses when the plan's own row changes have gone first.
#[tokio::test]
#[ignore = "needs a live SQL Server; run scripts/live-tests.sh"]
async fn a_new_foreign_key_is_probed_against_the_rows_the_plan_will_leave() {
    use pbps_dialect::Dialect;
    use pbps_model::{Cell, Change, ChangeSet, PlannedChange, RowKey, Value};

    let mut db = TestDb::create("fkprobe").await;
    db.conn
        .execute(
            "CREATE TABLE dbo.region (code varchar(10) NOT NULL CONSTRAINT pk_region PRIMARY KEY);
             CREATE TABLE dbo.customer (
                 code varchar(10) NOT NULL CONSTRAINT pk_customer PRIMARY KEY,
                 region_code varchar(10) NULL
             );
             INSERT INTO dbo.region VALUES ('here');
             INSERT INTO dbo.customer VALUES
                 ('stays',    'here'),
                 ('arriving', 'eu'),
                 ('repaired', 'nowhere'),
                 ('doomed',   'nowhere'),
                 ('exempt',   NULL);",
        )
        .await
        .expect("seed");

    let fk = Change::AddForeignKey {
        table: TableName::new("dbo", "customer"),
        name: "fk_customer_region".into(),
        constraint: Box::new(pbps_model::ForeignKey {
            columns: vec!["region_code".into()],
            references_table: TableName::new("dbo", "region"),
            references_columns: vec!["code".into()],
            on_delete: ReferentialAction::NoAction,
            on_update: ReferentialAction::NoAction,
        }),
    };
    let text = |s: &str| Cell::Value(Value::Text(s.to_owned()));

    // The three repairs the plan makes before the constraint is created: the
    // missing parent arrives, one orphan is pointed at it, and one goes.
    let repairs = vec![
        PlannedChange::new(Change::InsertRow {
            table: TableName::new("dbo", "region"),
            key_column: "code".into(),
            identity_key: false,
            key: RowKey::from("eu"),
            row: pbps_model::Row::default(),
            defaults: Default::default(),
            types: Default::default(),
        }),
        PlannedChange::new(Change::UpdateRow {
            table: TableName::new("dbo", "customer"),
            key_column: "code".into(),
            key: RowKey::from("repaired"),
            columns: [("region_code".to_owned(), (text("nowhere"), text("here")))]
                .into_iter()
                .collect(),
            unchanged: Default::default(),
            types: Default::default(),
            after_types: Default::default(),
        }),
        PlannedChange::new(Change::DeleteRow {
            table: TableName::new("dbo", "customer"),
            key_column: "code".into(),
            key: RowKey::from("doomed"),
            cause: pbps_model::change::DeleteCause::Undeclared,
            row: Default::default(),
            types: Default::default(),
            after_types: Default::default(),
        }),
    ];

    async fn count(conn: &mut Conn, cs: &ChangeSet) -> i32 {
        let probes = Mssql.preflight(cs);
        let probe = probes
            .iter()
            .find(|p| p.description.contains("parent"))
            .unwrap_or_else(|| panic!("no foreign-key probe in {probes:?}"));
        let rows = conn
            .query(&probe.sql)
            .await
            .unwrap_or_else(|e| panic!("the engine rejected a probe:\n{}\n{e}", probe.sql));
        rows[0].try_get_at(0).unwrap().unwrap()
    }

    // The constraint alone, against the data as it stands: three orphans.
    // `exempt` is NULL and the rule lets it through.
    let alone = ChangeSet {
        changes: vec![PlannedChange::new(fk.clone())],
    };
    assert_eq!(count(&mut db.conn, &alone).await, 3);

    // The same constraint in the plan that repairs the data first: none. This
    // is the count that used to be 3, refusing a plan the engine accepts.
    let mut whole = repairs.clone();
    whole.push(PlannedChange::new(fk.clone()));
    let whole = ChangeSet { changes: whole };
    assert_eq!(
        count(&mut db.conn, &whole).await,
        0,
        "a plan that repairs its own orphans must not be refused"
    );

    // And the engine agrees, which is the only thing that settles it: run the
    // plan's own statements in order and the constraint is created.
    apply(&mut db.conn, &whole).await;
    assert_eq!(count(&mut db.conn, &alone).await, 0, "the data is repaired");

    // The other direction, on the repaired table: a child row the plan itself
    // inserts, pointing nowhere, is counted — where a probe reading only what
    // is stored sees an empty table's worth of nothing.
    db.conn
        .execute("ALTER TABLE dbo.customer DROP CONSTRAINT fk_customer_region;")
        .await
        .expect("drop the constraint");
    let orphan_arriving = ChangeSet {
        changes: vec![
            PlannedChange::new(Change::InsertRow {
                table: TableName::new("dbo", "customer"),
                key_column: "code".into(),
                identity_key: false,
                key: RowKey::from("newcomer"),
                row: [("region_code".to_owned(), Value::Text("mars".into()))]
                    .into_iter()
                    .collect(),
                defaults: Default::default(),
                types: Default::default(),
            }),
            PlannedChange::new(fk),
        ],
    };
    assert_eq!(
        count(&mut db.conn, &orphan_arriving).await,
        1,
        "a row the plan inserts with no parent is one the constraint refuses"
    );

    // A parent this plan *creates*: the probe has no stored branch to take
    // its column names from, so a literal projection comes first. Unnamed, the
    // engine refuses the whole probe (Msg 8155) and preflight reports it as
    // unchecked — which under `apply --staged` means the table and its rows
    // commit before the constraint fails (DECISIONS 152).
    let mut lookup = Table::default();
    lookup
        .columns
        .insert("code".to_owned(), Column::new(ty("varchar(10)")).not_null());
    let to_a_new_parent = ChangeSet {
        changes: vec![
            PlannedChange::new(Change::CreateTable {
                uid: "t_bbbbbb".parse().unwrap(),
                name: TableName::new("dbo", "tier"),
                table: Box::new(lookup),
            }),
            PlannedChange::new(Change::InsertRow {
                table: TableName::new("dbo", "tier"),
                key_column: "code".into(),
                identity_key: false,
                key: RowKey::from("here"),
                row: pbps_model::Row::default(),
                defaults: Default::default(),
                types: Default::default(),
            }),
            PlannedChange::new(Change::AddForeignKey {
                table: TableName::new("dbo", "customer"),
                name: "fk_customer_tier".into(),
                constraint: Box::new(pbps_model::ForeignKey {
                    columns: vec!["region_code".into()],
                    references_table: TableName::new("dbo", "tier"),
                    references_columns: vec!["code".into()],
                    on_delete: ReferentialAction::NoAction,
                    on_update: ReferentialAction::NoAction,
                }),
            }),
        ],
    };
    // It runs at all, which is the half only the engine can settle, and it
    // counts correctly: `stays` and `repaired` both hold `here`, which the
    // plan inserts into the new table, `exempt` is NULL — and `arriving`
    // still holds `eu`, which the new table will not have.
    assert_eq!(count(&mut db.conn, &to_a_new_parent).await, 1);

    // And the type half, in the direction that makes the probe *pass* a plan
    // the engine refuses. The child column is `int` holding `1`; the plan
    // retypes it to `varchar` and moves the row to `01`, against a parent
    // holding `'1'`. As `int` the two are one value; as `varchar` they are
    // not, and it is the `varchar` the constraint will compare. Left to
    // `UNION ALL`'s own type precedence the planned `N'01'` becomes integer
    // `1`, the probe counts none, and the `ALTER TABLE ... ADD CONSTRAINT`
    // then fails (DECISIONS 152).
    db.conn
        .execute(
            "DELETE FROM dbo.customer;\n\
             DELETE FROM dbo.region;\n\
             ALTER TABLE dbo.customer ALTER COLUMN region_code int;\n\
             INSERT INTO dbo.region VALUES ('1');\n\
             INSERT INTO dbo.customer VALUES ('typed', 1);",
        )
        .await
        .expect("a key the two types spell differently");
    let retyping = ChangeSet {
        changes: vec![
            PlannedChange::new(Change::AlterColumnType {
                uid: "c_bbbbbb".parse().unwrap(),
                column: "dbo.customer.region_code".parse().unwrap(),
                from: ty("int"),
                to: ty("varchar(10)"),
                from_nullable: true,
                to_nullable: true,
            }),
            PlannedChange::new(Change::UpdateRow {
                table: TableName::new("dbo", "customer"),
                key_column: "code".into(),
                key: RowKey::from("typed"),
                columns: [("region_code".to_owned(), (text("1"), text("01")))]
                    .into_iter()
                    .collect(),
                unchanged: Default::default(),
                types: Default::default(),
                after_types: Default::default(),
            }),
            PlannedChange::new(Change::AddForeignKey {
                table: TableName::new("dbo", "customer"),
                name: "fk_customer_region2".into(),
                constraint: Box::new(pbps_model::ForeignKey {
                    columns: vec!["region_code".into()],
                    references_table: TableName::new("dbo", "region"),
                    references_columns: vec!["code".into()],
                    on_delete: ReferentialAction::NoAction,
                    on_update: ReferentialAction::NoAction,
                }),
            }),
        ],
    };
    assert_eq!(
        count(&mut db.conn, &retyping).await,
        1,
        "`01` is not `1` in the type the column will have"
    );
    // And the engine agrees, which is what makes the count the right one.
    let err = try_apply(&mut db.conn, &retyping)
        .await
        .expect_err("the constraint cannot be created over this row");
    assert!(err.contains("fk_customer_region2"), "{err}");

    // And a parent this plan creates with no declared rows at all: it will
    // hold none, so every non-NULL child reference is an orphan. Read as "no
    // question" this probe was not asked at all (DECISIONS 164).
    let mut bare = Table::default();
    bare.columns
        .insert("code".to_owned(), Column::new(ty("varchar(10)")).not_null());
    let to_an_empty_parent = ChangeSet {
        changes: vec![
            PlannedChange::new(Change::CreateTable {
                uid: "t_cccccc".parse().unwrap(),
                name: TableName::new("dbo", "band"),
                table: Box::new(bare),
            }),
            PlannedChange::new(Change::AddForeignKey {
                table: TableName::new("dbo", "customer"),
                name: "fk_customer_band".into(),
                constraint: Box::new(pbps_model::ForeignKey {
                    columns: vec!["region_code".into()],
                    references_table: TableName::new("dbo", "band"),
                    references_columns: vec!["code".into()],
                    on_delete: ReferentialAction::NoAction,
                    on_update: ReferentialAction::NoAction,
                }),
            }),
        ],
    };
    // `dbo.customer` holds one row whose `region_code` is non-NULL, and no
    // `dbo.band` will ever hold it.
    assert_eq!(count(&mut db.conn, &to_an_empty_parent).await, 1);

    db.drop().await;
}

/// An object name is spelled for `HAS_PERMS_BY_NAME` by the server, with
/// `QUOTENAME` on each part. Joined on the client with a dot and passed as
/// one string, `dbo.a.b` split at the wrong dot and answered 0, and
/// `dbo.x]y` did not parse and answered NULL — measured on the pinned image,
/// as `sa`, which holds `CONTROL` on both — so `doctor` reported a gap the
/// account did not have. Three names, one per shape the review named, on
/// each of the three object-scope questions; and an object that is not
/// there is still a gap, so the quoting did not turn absence into silence.
#[tokio::test]
#[ignore = "needs a live SQL Server; set PBPS_TEST_DB (see scripts/live-tests.sh)"]
async fn an_object_name_holding_a_dot_or_a_bracket_is_asked_about_as_named() {
    let mut db = TestDb::create("doctorquote").await;
    db.conn
        .execute(
            "CREATE TABLE dbo.[a.b] (id int NOT NULL PRIMARY KEY); \
             CREATE TABLE dbo.[x]]y] (id int NOT NULL PRIMARY KEY); \
             CREATE TABLE dbo.[order-items] (id int NOT NULL PRIMARY KEY); \
             CREATE ROLE app; \
             GRANT SELECT ON OBJECT::dbo.[a.b] TO app;",
        )
        .await
        .expect("create the oddly named tables and a role granted on one");

    let dot: pbps_model::ObjectName = pbps_model::ObjectName::new("dbo", "a.b");
    let bracket = pbps_model::ObjectName::new("dbo", "x]y");
    let hyphen = pbps_model::ObjectName::new("dbo", "order-items");
    let absent = pbps_model::ObjectName::new("dbo", "nope");
    let targets = pbps_mssql::doctor::GrantTargets {
        objects: vec![hyphen.clone(), absent.clone()],
        schemas: vec![],
        // The role's own grant on `dbo.[a.b]` reaches the question through
        // the catalog, spelled by `sys.objects`, not by these declarations.
        roles: vec!["app".to_owned()],
    };
    let held = pbps_mssql::doctor::permissions(
        &mut db.conn,
        &["dbo".to_owned()],
        std::slice::from_ref(&bracket),
        &targets,
        &pbps_mssql::doctor::DataTables::new(),
    )
    .await
    .expect("read permissions");

    for (name, map) in [
        (&dot, &held.granted_objects),
        (&hyphen, &held.granted_objects),
        (&bracket, &held.referenced_objects),
    ] {
        let granted = map
            .get(name)
            .unwrap_or_else(|| panic!("{name} was not asked about: {held:?}"));
        assert!(
            granted.contains("CONTROL") || granted.contains("REFERENCES"),
            "`sa` holds everything on {name}, and the question answered {granted:?}"
        );
    }
    let gaps = pbps_mssql::doctor::missing(&held);
    for name in [&dot, &hyphen, &bracket] {
        assert!(
            !gaps
                .iter()
                .any(|g| g.securable == pbps_mssql::doctor::Securable::Object(name.clone())),
            "a gap on a permission `sa` holds, on {name}: {gaps:?}"
        );
    }
    // Absent is still a gap: the object question asks every named target.
    assert!(
        gaps.iter().any(|g| g.permission == "CONTROL"
            && g.securable == pbps_mssql::doctor::Securable::Object(absent.clone())),
        "an object that is not there must still be reported: {gaps:?}"
    );

    db.drop().await;
}

/// A managed role granted on more tables than one statement can name. At
/// two bound slots per object, about a thousand objects crossed the
/// server's 2,100-parameter limit and the whole permission read failed, so
/// `doctor` reported `permission.unknown` for an estate it could have
/// checked. Asked in chunks, every table comes back, none twice, and `sa`
/// holds `CONTROL` on all of them.
#[tokio::test]
#[ignore = "needs a live SQL Server; set PBPS_TEST_DB (see scripts/live-tests.sh)"]
async fn a_role_granted_on_more_tables_than_one_statement_holds_is_read_whole() {
    let mut db = TestDb::create("doctorchunk").await;
    const TABLES: usize = 1_100;
    // One batch per hundred tables: a single batch of 1,100 statements is
    // slow to parse and says nothing the smaller ones do not.
    for start in (0..TABLES).step_by(100) {
        let mut batch = String::new();
        for i in start..(start + 100).min(TABLES) {
            batch.push_str(&format!(
                "CREATE TABLE dbo.[many_{i}] (id int NOT NULL PRIMARY KEY); "
            ));
        }
        db.conn.execute(&batch).await.expect("create tables");
    }
    let objects: Vec<pbps_model::ObjectName> = (0..TABLES)
        .map(|i| pbps_model::ObjectName::new("dbo", format!("many_{i}")))
        .collect();
    let targets = pbps_mssql::doctor::GrantTargets {
        objects: objects.clone(),
        schemas: vec![],
        roles: vec![],
    };
    let held = pbps_mssql::doctor::permissions(
        &mut db.conn,
        &["dbo".to_owned()],
        &objects,
        &targets,
        &pbps_mssql::doctor::DataTables::new(),
    )
    .await
    .expect("a list past one statement's worth of parameters must still be read");

    assert_eq!(
        held.granted_objects.len(),
        TABLES,
        "every declared target is asked about"
    );
    assert_eq!(
        held.referenced_objects.len(),
        TABLES,
        "and every referenced one"
    );
    for o in &objects {
        assert!(
            held.granted_objects[o].contains("CONTROL"),
            "`sa` holds CONTROL on {o}: {:?}",
            held.granted_objects[o]
        );
    }
    let gaps = pbps_mssql::doctor::missing(&held);
    assert!(
        !gaps
            .iter()
            .any(|g| matches!(g.securable, pbps_mssql::doctor::Securable::Object(_))),
        "no gap on an object `sa` holds everything on: {gaps:?}"
    );

    db.drop().await;
}

/// Where the parameter limit really sits, measured. The server says 2,100;
/// a statement bound through `sp_executesql` spends two of those on its own
/// `@stmt` and `@params`, so the last count a query may bind is 2,098.
/// `doctor::MAX_PARAMETERS` cites this test.
#[tokio::test]
#[ignore = "needs a live SQL Server; set PBPS_TEST_DB (see scripts/live-tests.sh)"]
async fn a_query_may_bind_two_fewer_parameters_than_the_server_names() {
    let mut db = TestDb::create("paramlimit").await;
    let probe = |n: usize| {
        let slots: Vec<String> = (1..=n).map(|i| format!("(@P{i})")).collect();
        let sql = format!(
            "SELECT COUNT(*) AS n FROM (VALUES {}) AS v(x);",
            slots.join(", ")
        );
        let params: Vec<pbps_db::Param<'static>> =
            (0..n).map(|_| pbps_db::Param::from("x")).collect();
        (sql, params)
    };
    let (sql, params) = probe(2098);
    db.conn
        .query_with(&sql, &params)
        .await
        .expect("2,098 bound parameters are accepted");
    let (sql, params) = probe(2099);
    let err = db
        .conn
        .query_with(&sql, &params)
        .await
        .err()
        .expect("2,099 are refused");
    assert!(format!("{err}").contains("2100"), "{err}");
    db.drop().await;
}

/// sysname's length belongs to the alias. Safe changes must preserve a value
/// at the source boundary, and shortening past it must still require approval.
#[tokio::test]
#[ignore = "needs a live SQL Server; run scripts/live-tests.sh"]
async fn sysname_risk_matches_the_engines_length_boundary() {
    use pbps_dialect::TypeChangeRisk;

    let mut db = TestDb::create("sysname_capacity").await;
    for (from, length, to, expected) in [
        ("sysname", 128, "nvarchar(10)", TypeChangeRisk::Narrowing),
        ("sysname", 128, "nvarchar(128)", TypeChangeRisk::Safe),
        ("nvarchar(50)", 50, "sysname", TypeChangeRisk::Safe),
        ("nvarchar(128)", 128, "sysname", TypeChangeRisk::Safe),
        ("nvarchar(129)", 129, "sysname", TypeChangeRisk::Narrowing),
    ] {
        db.conn
            .execute(&format!(
                "CREATE TABLE dbo.alias_capacity (v {from} NULL); \
             INSERT dbo.alias_capacity VALUES (REPLICATE(N'界', {length}));"
            ))
            .await
            .expect("create and fill source");
        let result = db
            .conn
            .execute(&format!(
                "ALTER TABLE dbo.alias_capacity ALTER COLUMN v {to} NULL;"
            ))
            .await;
        match expected {
            TypeChangeRisk::Safe => {
                result.unwrap_or_else(|e| panic!("{from} -> {to}: {e}"));
                let rows = db
                    .conn
                    .query("SELECT v FROM dbo.alias_capacity;")
                    .await
                    .unwrap();
                let value: &str = rows[0].try_get_at(0).unwrap().unwrap();
                assert_eq!(value, "界".repeat(length), "{from} -> {to}");
            }
            TypeChangeRisk::Narrowing => {
                let error = result.expect_err("the boundary value must not fit");
                assert_eq!(
                    error.server_error_code().as_deref(),
                    Some("2628"),
                    "{error}"
                );
            }
            TypeChangeRisk::Incompatible => unreachable!("these pairs are length changes"),
        }
        assert_eq!(
            Mssql.type_change_risk(&ty(from), &ty(to)),
            expected,
            "{from} -> {to}"
        );
        db.conn
            .execute("DROP TABLE dbo.alias_capacity;")
            .await
            .unwrap();
    }
    db.drop().await;
}

/// SQL equality can ignore the extra zero bytes, so compare the bytes sent
/// back to the application and their hash when judging whether data changed.
#[tokio::test]
#[ignore = "needs a live SQL Server; run scripts/live-tests.sh"]
async fn fixed_binary_growth_changes_the_payload_and_requires_approval() {
    use pbps_dialect::TypeChangeRisk;

    let mut db = TestDb::create("binary_padding").await;
    for (from, to, padded, risk) in [
        ("binary(8)", "binary(16)", true, TypeChangeRisk::Narrowing),
        ("binary(8)", "varbinary(16)", false, TypeChangeRisk::Safe),
        ("varbinary(8)", "varbinary(16)", false, TypeChangeRisk::Safe),
    ] {
        db.conn
            .execute(&format!(
                "CREATE TABLE dbo.binary_payload (v {from} NULL); \
             INSERT dbo.binary_payload VALUES (0x0102030405060708); \
             ALTER TABLE dbo.binary_payload ALTER COLUMN v {to} NULL;"
            ))
            .await
            .expect("alter binary payload");
        let rows = db
            .conn
            .query(
                "SELECT CONVERT(varchar(max),v,2), DATALENGTH(v), \
             CASE WHEN HASHBYTES('SHA2_256',v) = HASHBYTES('SHA2_256',0x0102030405060708) \
                  THEN 1 ELSE 0 END, \
             CASE WHEN v = 0x0102030405060708 THEN 1 ELSE 0 END \
             FROM dbo.binary_payload;",
            )
            .await
            .unwrap();
        let payload: &str = rows[0].try_get_at(0).unwrap().unwrap();
        let bytes: i32 = rows[0].try_get_at(1).unwrap().unwrap();
        let same_hash: i32 = rows[0].try_get_at(2).unwrap().unwrap();
        let equal: i32 = rows[0].try_get_at(3).unwrap().unwrap();
        assert_eq!(
            payload,
            if padded {
                "01020304050607080000000000000000"
            } else {
                "0102030405060708"
            }
        );
        assert_eq!(bytes, if padded { 16 } else { 8 });
        assert_eq!(same_hash, i32::from(!padded));
        assert_eq!(
            equal, 1,
            "SQL equality alone does not prove byte preservation"
        );
        assert_eq!(
            Mssql.type_change_risk(&ty(from), &ty(to)),
            risk,
            "{from} -> {to}"
        );
        db.conn
            .execute("DROP TABLE dbo.binary_payload;")
            .await
            .unwrap();
    }
    db.drop().await;
}

/// The pull hides this tool's own two tables and nothing else that looks like
/// them. `NOT LIKE '\_\_pbps\_%'` also hid `dbo.__pbps_customers`, and the bare
/// names still hid `app.__pbps_state`; no rule refuses either declaration, so
/// the pull reported a table that is there as absent and the next plan tried to
/// create it again (issue #170).
///
/// The ledger here is the real one, created by `ensure_tables` rather than by
/// hand, so the test asserts against the names this tool actually installs.
#[tokio::test]
#[ignore = "needs a live SQL Server; set PBPS_TEST_DB (see scripts/live-tests.sh)"]
async fn the_pull_hides_this_tools_two_tables_and_not_a_projects_own() {
    let mut db = TestDb::create("ourtables").await;
    pbps_mssql::state::ensure_tables(&mut db.conn)
        .await
        .expect("create the ledger");
    // Its own batch: SQL Server wants `CREATE SCHEMA` first in one.
    db.conn
        .execute("CREATE SCHEMA app;")
        .await
        .expect("create the schema");
    db.conn
        .execute(
            "CREATE TABLE dbo.__pbps_customers (id int NOT NULL);
             -- The ledger's own names, in a schema that is not the ledger's.
             CREATE TABLE app.__pbps_state (id int NOT NULL);
             CREATE TABLE app.__pbps_lock (id int NOT NULL);",
        )
        .await
        .expect("create a project's own tables");

    let pulled = pbps_mssql::catalog::introspect(&mut db.conn)
        .await
        .expect("introspect");
    db.drop().await;

    let names = pulled.schema.tables.keys().cloned().collect::<Vec<_>>();
    for theirs in [
        TableName::new("dbo", "__pbps_customers"),
        TableName::new("app", "__pbps_state"),
        TableName::new("app", "__pbps_lock"),
    ] {
        assert!(
            names.contains(&theirs),
            "a project's own table must be in the pull: {theirs:?} in {names:?}"
        );
    }
    // The negative case, and the reason the filter exists: introspecting the
    // ledger would make the first plan after a snapshot propose dropping it.
    assert!(
        !names.contains(&TableName::new("dbo", "__pbps_state"))
            && !names.contains(&TableName::new("dbo", "__pbps_lock")),
        "this tool's own tables must never enter the managed set: {names:?}"
    );
}

/// A column that has a default and changes type is three statements, in one
/// order: the old default out, the type changed, the new default in.
///
/// This engine is where the middle phase is forced. **Measured**, `ALTER TABLE
/// … ALTER COLUMN n bigint` is refused while any default constraint stands on
/// the column — 5074, "the object 'df_dn2' is dependent on column 'n'", with
/// 4922 behind it — and the *nullability* form of the same statement is
/// accepted, so the dependency belongs to the type change rather than to
/// `ALTER COLUMN`. PostgreSQL accepts the type change and refuses the other
/// end, a default the old type cannot read, which is why the ordering is the
/// differ's and not a dialect's (DECISIONS 271).
///
/// The plan is run through `try_apply`, in one transaction, because that is
/// how a refusal reaches an operator: the statement and the engine's word, and
/// nothing before it left standing.
#[tokio::test]
#[ignore = "needs a live SQL Server; run scripts/live-tests.sh"]
async fn a_retyped_column_gives_up_its_old_default_before_the_type_moves() {
    let with = |t: &str, d: &str| Column {
        default: Some(d.to_owned()),
        ..Column::new(ty(t))
    };
    let table_of = |c: Column| {
        let mut t = Table::default();
        t.columns.insert("n".into(), c);
        let mut s = Schema::default();
        s.tables.insert(TableName::new("dbo", "retyped"), t);
        s
    };
    let a = table_of(with("int", "40000"));
    let b = table_of(with("smallint", "1"));
    let ids_a = mint_ids(&a, &IdsFile::default(), &[]);
    let ids_b = mint_ids(&b, &ids_a, &[]);

    let mut db = TestDb::create("retyped").await;
    apply(
        &mut db.conn,
        &plan(&Schema::default(), &IdsFile::default(), &a, &ids_a),
    )
    .await;
    let refused = try_apply(&mut db.conn, &plan(&a, &ids_a, &b, &ids_b)).await;
    let state = pbps_mssql::catalog::introspect(&mut db.conn)
        .await
        .expect("introspect");
    db.drop().await;

    assert_eq!(refused, Ok(()), "the plan has to apply");
    assert_eq!(state.schema, normalized(&b));
}

/// A declared expression ending in a line comment reaches the engine with the
/// emitter's own syntax intact.
///
/// The declaration is valid — measured, SQL Server accepts every one of these
/// with its closer on the next line and refuses it on the same one — so
/// without the newline this is a valid plan the tool cannot apply, and the
/// refusal arrives from the server at `apply` rather than from the gate.
///
/// What is pinned is that all three expressions arrived and none of them took
/// the syntax behind it away. It cannot be pinned by reading the comments back:
/// the engine stores the parsed expression, so `DEFAULT (0 -- why)` comes back
/// as `((0))` with the comment gone. Only running the statements shows this.
#[tokio::test]
#[ignore = "needs a live SQL Server; run scripts/live-tests.sh"]
async fn an_expression_ending_in_a_line_comment_still_applies() {
    let mut t = Table::default();
    t.columns
        .insert("id".into(), Column::new(ty("int")).not_null());
    let mut status = Column::new(ty("tinyint")).not_null();
    status.default = Some("0 -- the default status".into());
    t.columns.insert("status".into(), status);
    t.columns
        .insert("amount".into(), Column::new(ty("int")).not_null());
    t.primary_key = Some(PrimaryKey {
        name: Some("pk_commented".into()),
        columns: vec!["id".into()],
    });
    t.checks.insert(
        "ck_commented_amount".into(),
        pbps_model::CheckConstraint {
            expression: "[amount]>=(0) -- never negative".into(),
        },
    );
    t.indexes.insert(
        "ix_commented_status".into(),
        Index {
            columns: vec![IndexColumn {
                name: "status".into(),
                descending: false,
            }],
            include: Vec::new(),
            unique: false,
            filter: Some("[status] IS NOT NULL -- only the set ones".into()),
        },
    );
    let mut declared = Schema::default();
    declared
        .tables
        .insert(TableName::new("dbo", "commented"), t);
    let ids = mint_ids(&declared, &IdsFile::default(), &[]);

    // And the same expressions through the sites a *later* plan uses: a
    // default added on its own, and a check added on its own.
    let mut later = declared.clone();
    let table = later
        .tables
        .get_mut(&TableName::new("dbo", "commented"))
        .expect("table");
    table.columns.get_mut("amount").expect("column").default =
        Some("1 -- and this one is added later".into());
    table.checks.insert(
        "ck_commented_status".into(),
        pbps_model::CheckConstraint {
            expression: "[status]>=(0) -- added later too".into(),
        },
    );
    let ids_later = mint_ids(&later, &ids, &[]);

    let mut db = TestDb::create("commented").await;
    let bootstrap = try_apply(
        &mut db.conn,
        &plan(&Schema::default(), &IdsFile::default(), &declared, &ids),
    )
    .await;
    let added = try_apply(&mut db.conn, &plan(&declared, &ids, &later, &ids_later)).await;
    let state = pbps_mssql::catalog::introspect(&mut db.conn)
        .await
        .expect("introspect");
    db.drop().await;

    assert_eq!(bootstrap, Ok(()), "the bootstrap has to apply");
    assert_eq!(added, Ok(()), "the later plan has to apply");
    // The engine keeps none of the comments, so what came back is the parsed
    // expression — three of them, which is the point: each arrived whole.
    let pulled = state
        .schema
        .tables
        .get(&TableName::new("dbo", "commented"))
        .expect("the table was created");
    assert_eq!(
        pulled.columns["status"].default.as_deref(),
        Some("0"),
        "the column default arrived"
    );
    assert_eq!(
        pulled.columns["amount"].default.as_deref(),
        Some("1"),
        "the default added later arrived"
    );
    assert_eq!(
        pulled.checks["ck_commented_amount"].expression, "[amount]>=(0)",
        "the check arrived"
    );
    assert_eq!(
        pulled.checks["ck_commented_status"].expression, "[status]>=(0)",
        "the check added later arrived"
    );
    assert_eq!(
        pulled.indexes["ix_commented_status"].filter.as_deref(),
        Some("[status] IS NOT NULL"),
        "the index filter arrived"
    );
}

/// `timestamp` is eight bytes and is not a binary column: the engine refuses
/// `ALTER COLUMN` on either end of it, by number.
///
/// Both errors are here because they are two different refusals — 4928 names
/// the column's *current* type and 4927 names the one asked for — and a fix
/// that caught only one direction would pass a test that asked only about the
/// other.
///
/// The last assertion is the reason `preflight` builds no probe for these:
/// the conversion a probe would ask about *succeeds*. `TRY_CONVERT` finds
/// nothing wrong with the values, so a probe over them counts zero rows and
/// prints as a pass under a statement that will not compile. The prohibition
/// is on the column, not on the data, and only the classification can carry
/// it.
#[tokio::test]
#[ignore = "needs a live SQL Server; run scripts/live-tests.sh"]
async fn a_timestamp_column_cannot_be_altered_into_or_out_of() {
    use pbps_dialect::TypeChangeRisk;

    let mut db = TestDb::create("timestamp_alter").await;
    db.conn
        .execute(
            "CREATE TABLE dbo.rv (id int NOT NULL, v rowversion, b varbinary(8) NULL); \
             INSERT dbo.rv (id, b) VALUES (1, 0xAB);",
        )
        .await
        .expect("create the probe table");

    // Out of the type. `varbinary(8)` is the exact capacity `sys.types`
    // reports for `timestamp`, which is the pair a capacity-based classifier
    // calls Safe.
    let error = db
        .conn
        .execute("ALTER TABLE dbo.rv ALTER COLUMN v varbinary(8) NULL;")
        .await
        .expect_err("SQL Server does not alter a timestamp column");
    assert_eq!(
        error.server_error_code().as_deref(),
        Some("4928"),
        "{error}"
    );

    // Into it.
    let error = db
        .conn
        .execute("ALTER TABLE dbo.rv ALTER COLUMN b timestamp;")
        .await
        .expect_err("SQL Server does not alter a column into timestamp");
    assert_eq!(
        error.server_error_code().as_deref(),
        Some("4927"),
        "{error}"
    );

    // Restating the same type only to change nullability is still an ALTER
    // COLUMN that names `timestamp`, so both of its spellings are refused.
    for statement in [
        "ALTER TABLE dbo.rv ALTER COLUMN v timestamp NOT NULL;",
        "ALTER TABLE dbo.rv ALTER COLUMN v rowversion NULL;",
    ] {
        let error = db
            .conn
            .execute(statement)
            .await
            .expect_err("SQL Server refuses a nullability-only timestamp alteration");
        assert_eq!(
            error.server_error_code().as_deref(),
            Some("4927"),
            "{statement}: {error}"
        );
    }

    // CREATE accepts and stores the nullable spelling. The declaration is
    // valid; only a later attempt to change its nullability is impossible.
    db.conn
        .execute("CREATE TABLE dbo.rv_nullable (id int NOT NULL, v rowversion NULL);")
        .await
        .expect("SQL Server accepts rowversion NULL at CREATE time");
    let rows = db
        .conn
        .query(
            "SELECT c.is_nullable FROM sys.columns c \
             WHERE c.object_id = OBJECT_ID('dbo.rv_nullable') AND c.name = 'v';",
        )
        .await
        .unwrap();
    let nullable: bool = rows[0].try_get_at(0).unwrap().unwrap();
    assert!(
        nullable,
        "SQL Server stores an explicitly nullable rowversion"
    );

    // Nothing moved: a refusal that left the column half-changed would make
    // the risk classification the least of the problems.
    let rows = db
        .conn
        .query(
            "SELECT t.name FROM sys.columns c JOIN sys.types t \
             ON t.user_type_id = c.user_type_id \
             WHERE c.object_id = OBJECT_ID('dbo.rv') AND c.name = 'v';",
        )
        .await
        .unwrap();
    let name: &str = rows[0].try_get_at(0).unwrap().unwrap();
    assert_eq!(name, "timestamp");

    // What the dialect says about the same two changes, and about the identity
    // — which is not a change and must stay Safe however it is spelled, or
    // every plan against a table with a `rowversion` grows a change nobody
    // asked for.
    for (from, to, expected) in [
        ("timestamp", "varbinary(8)", TypeChangeRisk::Incompatible),
        ("varbinary(8)", "timestamp", TypeChangeRisk::Incompatible),
        ("rowversion", "varbinary(8)", TypeChangeRisk::Incompatible),
        ("timestamp", "timestamp", TypeChangeRisk::Safe),
        ("rowversion", "timestamp", TypeChangeRisk::Safe),
    ] {
        assert_eq!(
            Mssql.type_change_risk(&ty(from), &ty(to)),
            expected,
            "{from} -> {to}"
        );
    }

    // The conversion is legal; the alteration is not. This is why no probe can
    // stand in for the classification — the query below is exactly the one
    // `conversion_probe` used to build for this change, and it answers zero.
    let rows = db
        .conn
        .query(
            "SELECT COUNT(*) AS n FROM dbo.rv \
             WHERE b IS NOT NULL AND TRY_CONVERT(timestamp, b) IS NULL;",
        )
        .await
        .unwrap();
    let unconvertible: i32 = rows[0].try_get_at(0).unwrap().unwrap();
    assert_eq!(
        unconvertible, 0,
        "the probe was supposed to be useless here; if it can see the \
         prohibition, the classification is not the only thing carrying it"
    );

    db.conn
        .execute("DROP TABLE dbo.rv_nullable; DROP TABLE dbo.rv;")
        .await
        .unwrap();
    db.drop().await;
}

/// Digit counts hide both integer overflow and fractional binary rounding.
#[tokio::test]
#[ignore = "needs a live SQL Server; run scripts/live-tests.sh"]
async fn numeric_capacity_requires_approval_for_overflow_and_changed_values() {
    use pbps_dialect::TypeChangeRisk;
    let mut db = TestDb::create("numeric_loss").await;
    for (from, to, value) in [
        ("decimal(3,0)", "tinyint", "999"),
        ("decimal(2,0)", "tinyint", "-1"),
        ("decimal(5,0)", "smallint", "99999"),
        ("decimal(10,0)", "int", "9999999999"),
        ("decimal(19,0)", "bigint", "9999999999999999999"),
        ("decimal(10,4)", "smallmoney", "999999.9999"),
        ("decimal(19,4)", "money", "999999999999999.9999"),
    ] {
        db.conn.execute(&format!(
            "CREATE TABLE dbo.numeric_loss (v {from}); INSERT dbo.numeric_loss VALUES ({value});"
        )).await.unwrap();
        db.conn
            .execute(&format!(
                "ALTER TABLE dbo.numeric_loss ALTER COLUMN v {to};"
            ))
            .await
            .expect_err("the source boundary must overflow the target");
        assert_eq!(
            Mssql.type_change_risk(&ty(from), &ty(to)),
            TypeChangeRisk::Narrowing,
            "{from} -> {to}"
        );
        db.conn
            .execute("DROP TABLE dbo.numeric_loss;")
            .await
            .unwrap();
    }
    for (from, to, value) in [
        ("decimal(2,1)", "real", "0.1"),
        ("decimal(8,0)", "real", "16777217"),
        ("decimal(16,0)", "float", "9007199254740993"),
    ] {
        db.conn.execute(&format!(
            "CREATE TABLE dbo.numeric_loss (v {from}); INSERT dbo.numeric_loss VALUES ({value});              ALTER TABLE dbo.numeric_loss ALTER COLUMN v {to};"
        )).await.unwrap();
        // Compare in an exact domain: comparing to a float would convert the
        // expected value to the same rounded float and conceal the loss.
        let rows = db.conn.query(&format!(
            "SELECT CAST(CASE WHEN CONVERT(decimal(38,20), v) =              CONVERT(decimal(38,20), {value}) THEN 1 ELSE 0 END AS int) FROM dbo.numeric_loss;"
        )).await.unwrap();
        assert_eq!(
            rows[0].try_get_at::<i32>(0).unwrap(),
            Some(0),
            "{from} -> {to}"
        );
        assert_eq!(
            Mssql.type_change_risk(&ty(from), &ty(to)),
            TypeChangeRisk::Narrowing
        );
        db.conn
            .execute("DROP TABLE dbo.numeric_loss;")
            .await
            .unwrap();
    }
    db.drop().await;
}

#[tokio::test]
#[ignore = "needs a live SQL Server; run scripts/live-tests.sh"]
async fn numeric_capacity_safe_alters_preserve_both_endpoints() {
    use pbps_dialect::TypeChangeRisk;
    let mut db = TestDb::create("numeric_safe").await;
    for (from, to, min, max) in [
        ("bit", "tinyint", "0", "1"),
        ("tinyint", "decimal(3,0)", "0", "255"),
        ("decimal(4,0)", "smallint", "-9999", "9999"),
        ("decimal(9,0)", "int", "-999999999", "999999999"),
        (
            "decimal(18,0)",
            "bigint",
            "-999999999999999999",
            "999999999999999999",
        ),
        (
            "bigint",
            "decimal(19,0)",
            "-9223372036854775808",
            "9223372036854775807",
        ),
        ("decimal(9,4)", "smallmoney", "-99999.9999", "99999.9999"),
        (
            "decimal(18,4)",
            "money",
            "-99999999999999.9999",
            "99999999999999.9999",
        ),
        ("smallmoney", "money", "-214748.3648", "214748.3647"),
        (
            "money",
            "decimal(19,4)",
            "-922337203685477.5808",
            "922337203685477.5807",
        ),
        ("smallint", "real", "-32768", "32767"),
        ("int", "float", "-2147483648", "2147483647"),
        ("decimal(7,0)", "real", "-9999999", "9999999"),
        (
            "decimal(15,0)",
            "float",
            "-999999999999999",
            "999999999999999",
        ),
    ] {
        assert_eq!(
            Mssql.type_change_risk(&ty(from), &ty(to)),
            TypeChangeRisk::Safe,
            "{from} -> {to}"
        );
        db.conn.execute(&format!(
            "CREATE TABLE dbo.numeric_safe (v {from}, expected decimal(38,4));              INSERT dbo.numeric_safe VALUES ({min}, {min}), ({max}, {max});              ALTER TABLE dbo.numeric_safe ALTER COLUMN v {to};"
        )).await.unwrap_or_else(|e| panic!("{from} -> {to}: {e}"));
        let rows = db.conn.query(
            "SELECT COUNT(*) FROM dbo.numeric_safe WHERE CONVERT(decimal(38,4), v) <> expected;"
        ).await.unwrap();
        assert_eq!(
            rows[0].try_get_at::<i32>(0).unwrap(),
            Some(0),
            "{from} -> {to}"
        );
        db.conn
            .execute("DROP TABLE dbo.numeric_safe;")
            .await
            .unwrap();
    }
    db.drop().await;
}
