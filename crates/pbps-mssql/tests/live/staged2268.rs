//! Staged comparison through the public capture API, on two real SQL Servers.

use super::connect_live;
use pbps_db::Conn;
use pbps_mssql::resolver::capture::{self, StageError};
use std::collections::BTreeSet;

struct Fixture {
    target: Conn,
    scratch: Conn,
}

async fn case<F, Fut>(tag: &str, run: F)
where
    F: FnOnce(Fixture) -> Fut,
    Fut: Future<Output = ()> + Send + 'static,
{
    let target = std::env::var("PBPS_TEST_DB").unwrap();
    let scratch = std::env::var("PBPS_TEST_MSSQL_SCRATCH_DB").unwrap();
    let name = format!("pbps2268_{tag}_{}", std::process::id());
    for server in [&target, &scratch] {
        super::create_database_apart(server, &name).await;
    }
    let fixture = Fixture {
        target: connect_live(&format!("{target};Database={name}"))
            .await
            .unwrap(),
        scratch: connect_live(&format!("{scratch};Database={name}"))
            .await
            .unwrap(),
    };
    let result = tokio::spawn(run(fixture)).await;
    for server in [&target, &scratch] {
        connect_live(server).await.unwrap().execute(&format!("ALTER DATABASE [{name}] SET SINGLE_USER WITH ROLLBACK IMMEDIATE; DROP DATABASE [{name}];")).await.unwrap();
    }
    result.unwrap();
}

fn roots(names: &[&str]) -> BTreeSet<capture::ObjectIdentity> {
    names.iter().map(|n| capture::object("dbo", n)).collect()
}

impl Fixture {
    async fn both(&mut self, sql: &str) {
        self.target.execute(sql).await.unwrap();
        self.scratch.execute(sql).await.unwrap();
    }
    async fn compare(&mut self, names: &[&str]) -> Result<capture::Staged, StageError> {
        capture::compare_staged(&mut self.scratch, &mut self.target, &roots(names), &[]).await
    }
}

fn refused(result: Result<capture::Staged, StageError>, name: &str) {
    let error = result.unwrap_err();
    match error {
        StageError::Refused(findings) => {
            assert!(findings.iter().any(|f| f.contains(name)), "{findings:?}")
        }
        other => panic!("{other:?}"),
    }
}

fn qualified(result: Result<capture::Staged, StageError>) {
    match result {
        Err(StageError::Capture(capture::CaptureError::Coverage(uncovered))) => {
            assert_eq!(
                uncovered.condition,
                "a reference names an object in another database or server"
            );
            assert!(uncovered.object.is_some());
        }
        other => panic!("the named multipart boundary did not refuse: {other:?}"),
    }
}

#[tokio::test]
#[ignore]
async fn external_nullability_and_type_differences_refuse() {
    case("shape", |mut f| async move {
        f.both("CREATE TABLE dbo.t(a int NULL);").await;
        f.compare(&["t"]).await.unwrap();
        f.scratch
            .execute("ALTER TABLE dbo.t ALTER COLUMN a int NOT NULL;")
            .await
            .unwrap();
        refused(f.compare(&["t"]).await, "a");
        f.scratch
            .execute("ALTER TABLE dbo.t ALTER COLUMN a bigint NULL;")
            .await
            .unwrap();
        refused(f.compare(&["t"]).await, "a");
        f.scratch
            .execute("ALTER TABLE dbo.t ALTER COLUMN a int NULL;")
            .await
            .unwrap();
        f.compare(&["t"]).await.unwrap();
    })
    .await;
}

#[tokio::test]
#[ignore]
async fn five_hundred_rows_do_not_change_external_shape() {
    case("rows", |mut f| async move {
        f.both("CREATE TABLE dbo.t(a int IDENTITY(1,1) CONSTRAINT t_pk PRIMARY KEY,b int);")
            .await;
        let scope = capture::CaptureScope {
            retained: roots(&["t"]),
            ..Default::default()
        };
        let before = capture::capture(&mut f.target, &scope).await.unwrap();
        f.target
            .execute("INSERT dbo.t(b) SELECT TOP(500) 1 FROM sys.all_objects;")
            .await
            .unwrap();
        let after = capture::capture(&mut f.target, &scope).await.unwrap();
        assert!(
            before.compare(&after).is_empty(),
            "data must not change the canonical sealed shape"
        );
        f.compare(&["t"]).await.unwrap();
        f.scratch
            .execute("ALTER TABLE dbo.t ALTER COLUMN b bigint;")
            .await
            .unwrap();
        refused(f.compare(&["t"]).await, "b");
    })
    .await;
}

#[tokio::test]
#[ignore]
async fn skipped_child_aggregate_flags_do_not_change_shape() {
    case("children", |mut f| async move {
        f.both("CREATE TABLE dbo.p(a int CONSTRAINT p_pk PRIMARY KEY); CREATE TABLE dbo.t(a int,b int);").await;
        for ddl in [
            "ALTER TABLE dbo.t ADD CONSTRAINT ck CHECK(a>0); ALTER TABLE dbo.t ADD CONSTRAINT df DEFAULT 1 FOR b; ALTER TABLE dbo.t ADD CONSTRAINT fk FOREIGN KEY(a) REFERENCES dbo.p(a);",
            "CREATE INDEX ix ON dbo.t(b);",
            "CREATE CLUSTERED INDEX cx ON dbo.t(a);",
            "CREATE TRIGGER dbo.tr ON dbo.t AFTER INSERT AS RETURN;",
            "ALTER TRIGGER dbo.tr ON dbo.t AFTER UPDATE AS RETURN;",
            "ALTER TRIGGER dbo.tr ON dbo.t AFTER DELETE AS RETURN;",
            "ALTER TRIGGER dbo.tr ON dbo.t INSTEAD OF INSERT AS RETURN;",
            "ALTER TRIGGER dbo.tr ON dbo.t INSTEAD OF UPDATE AS RETURN;",
            "ALTER TRIGGER dbo.tr ON dbo.t INSTEAD OF DELETE AS RETURN;",
        ] {
            f.target.execute(ddl).await.unwrap();
            f.compare(&["t", "p"]).await.unwrap();
        }
        f.both("CREATE TABLE dbo.has_clustered(a int CONSTRAINT clustered_pk PRIMARY KEY CLUSTERED,b int); CREATE TABLE dbo.has_nonclustered(a int CONSTRAINT nonclustered_pk PRIMARY KEY NONCLUSTERED,b int);").await;
        f.target.execute("CREATE INDEX sibling_nc ON dbo.has_clustered(b); CREATE CLUSTERED INDEX sibling_c ON dbo.has_nonclustered(b);").await.unwrap();
        f.compare(&["has_clustered","has_nonclustered"]).await.unwrap();
        f.target.execute("CREATE UNIQUE INDEX uq ON dbo.t(a);").await.unwrap();
        refused(f.compare(&["t", "p"]).await, "t");
    }).await;
}

#[tokio::test]
#[ignore]
async fn bulk_lock_option_is_skipped_in_every_representation() {
    case("bulk", |mut f| async move {
        f.both("CREATE TABLE dbo.t(a int);").await;
        f.target
            .execute("EXEC sys.sp_tableoption N'dbo.t',N'table lock on bulk load',N'ON';")
            .await
            .unwrap();
        f.compare(&["t"]).await.unwrap();
        f.target
            .execute("ALTER TABLE dbo.t ALTER COLUMN a bigint;")
            .await
            .unwrap();
        refused(f.compare(&["t"]).await, "a");
    })
    .await;
}

#[tokio::test]
#[ignore]
async fn unknown_scalar_catalog_property_is_compared() {
    case("scalar", |mut f| async move {
        f.both("CREATE TABLE dbo.t(a text);").await;
        f.compare(&["t"]).await.unwrap();
        // This catalog scalar has no bespoke normalized field in select.rs.
        f.target
            .execute("EXEC sys.sp_tableoption N'dbo.t',N'text in row',N'100';")
            .await
            .unwrap();
        refused(f.compare(&["t"]).await, "t");
    })
    .await;
}

#[tokio::test]
#[ignore]
async fn module_body_can_differ_but_compile_flags_cannot() {
    case("body", |mut f| async move {
        f.both("CREATE VIEW dbo.v AS SELECT CONVERT(int,1) AS a;")
            .await;
        f.scratch
            .execute("ALTER VIEW dbo.v AS SELECT CONVERT(int,2) AS a;")
            .await
            .unwrap();
        f.compare(&["v"]).await.unwrap();
        f.scratch
            .execute("SET QUOTED_IDENTIFIER OFF;")
            .await
            .unwrap();
        f.scratch
            .execute("ALTER VIEW dbo.v AS SELECT CONVERT(int,2) AS a;")
            .await
            .unwrap();
        refused(f.compare(&["v"]).await, "v");
    })
    .await;
}

#[tokio::test]
#[ignore]
async fn determinism_precision_and_data_access_are_compared() {
    case("semantics", |mut f| async move {
        f.both("CREATE TABLE dbo.t(a int);").await;
        for (target, scratch) in [
            ("RETURN @a;", "RETURN @a+DATEPART(second,GETDATE());"),
            ("RETURN @a;", "RETURN CONVERT(int,CONVERT(float,@a)/3.0);"),
            ("RETURN @a;", "DECLARE @n int; SELECT @n=MAX(a) FROM dbo.t; RETURN @n;"),
            ("RETURN @a;", "DECLARE @n int; SELECT @n=COUNT(*) FROM sys.objects; RETURN @n;"),
        ] {
            let binding = if scratch.contains("sys.objects") { "" } else { "WITH SCHEMABINDING" };
            for (conn, body, binding) in [(&mut f.target,target,"WITH SCHEMABINDING"), (&mut f.scratch,scratch,binding)] {
                conn.execute(&format!("CREATE OR ALTER FUNCTION dbo.f(@a int) RETURNS int {binding} AS BEGIN {body} END;")).await.unwrap();
            }
            refused(f.compare(&["f"]).await, "OBJECTPROPERTYEX");
        }
        f.target.execute("CREATE OR ALTER FUNCTION dbo.f(@a int) RETURNS int WITH SCHEMABINDING AS BEGIN RETURN @a; END;").await.unwrap();
        f.scratch.execute("CREATE OR ALTER FUNCTION dbo.f(@a int) RETURNS int WITH SCHEMABINDING AS BEGIN RETURN @a+1; END;").await.unwrap();
        f.compare(&["f"]).await.unwrap();
    }).await;
}

#[tokio::test]
#[ignore]
async fn null_property_is_not_an_absent_object() {
    case("null", |mut f| async move {
        f.both("CREATE TABLE dbo.t(a int);").await;
        let rows = f.target.query("SELECT CONVERT(nvarchar(20),OBJECTPROPERTYEX(OBJECT_ID(N'dbo.t'),N'IsPrecise')) AS value;").await.unwrap();
        assert_eq!(rows[0].try_get::<&str>("value").unwrap(), None);
        f.compare(&["t"]).await.unwrap();
        f.target.execute("DROP TABLE dbo.t;").await.unwrap();
        refused(f.compare(&["t"]).await, "t");
    }).await;
}

#[tokio::test]
#[ignore]
async fn staged_external_change_after_acceptance_is_detected() {
    case("change", |mut f| async move {
        f.both("CREATE TABLE dbo.t(a int);").await;
        let staged = f.compare(&["t"]).await.unwrap();
        let current = capture::capture(&mut f.target, staged.scope())
            .await
            .unwrap();
        assert!(staged.changed_since(&current).is_empty());
        f.target
            .execute("ALTER TABLE dbo.t ALTER COLUMN a bigint;")
            .await
            .unwrap();
        let current = capture::capture(&mut f.target, staged.scope())
            .await
            .unwrap();
        assert!(!staged.changed_since(&current).is_empty());
    })
    .await;
}

#[tokio::test]
#[ignore]
async fn qualified_user_object_references_refuse() {
    case("qualified", |mut f| async move {
        f.both("CREATE TABLE dbo.t(a int);").await;
        for source in [
            "CREATE PROCEDURE dbo.p AS SELECT * FROM missing2268.dbo.t;",
            "CREATE PROCEDURE dbo.p AS SELECT * FROM msdb.dbo.sysjobs;",
        ] {
            f.target.execute(source).await.unwrap();
            qualified(f.compare(&["p"]).await);
            f.target.execute("DROP PROCEDURE dbo.p;").await.unwrap();
        }
        let row = f.target.query("SELECT QUOTENAME(CONVERT(nvarchar(128),SERVERPROPERTY('ServerName'))) AS server_name, QUOTENAME(DB_NAME()) AS database_name;").await.unwrap();
        let server = row[0].try_get::<&str>("server_name").unwrap().unwrap().to_owned();
        let database = row[0].try_get::<&str>("database_name").unwrap().unwrap().to_owned();
        for source in [format!("CREATE VIEW dbo.v AS SELECT a FROM {database}.dbo.t;"), format!("CREATE VIEW dbo.v AS SELECT a FROM {server}.{database}.dbo.t;")] {
            f.target.execute(&source).await.unwrap();
            qualified(f.compare(&["v"]).await);
            f.target.execute("DROP VIEW dbo.v;").await.unwrap();
        }
    }).await;
}

#[tokio::test]
#[ignore]
async fn qualified_synonym_refuses() {
    case("synonym", |mut f| async move {
        f.both("CREATE SYNONYM dbo.s FOR master.sys.objects;").await;
        qualified(f.compare(&["s"]).await);
        f.both("DROP SYNONYM dbo.s; CREATE TABLE dbo.[t.dot](a int); CREATE SYNONYM dbo.s FOR dbo.[t.dot];").await;
        f.compare(&["s"]).await.unwrap();
    }).await;
}

#[tokio::test]
#[ignore]
async fn two_part_reference_is_accepted() {
    case("local", |mut f| async move {
        f.both("CREATE TABLE dbo.[t.dot](a int);").await;
        f.both("CREATE VIEW dbo.v AS SELECT dbo.[t.dot].a FROM dbo.[t.dot];")
            .await;
        f.compare(&["t.dot", "v"]).await.unwrap();
        // The documented residual and dynamic SQL are outside static dependency
        // coverage. No source parser rejects a string, comment or local column.
        f.both("CREATE PROCEDURE dbo.p AS EXEC(N'SELECT * FROM master.sys.objects');")
            .await;
        f.compare(&["p"]).await.unwrap();
        f.both("CREATE VIEW dbo.system_view AS SELECT name FROM master.sys.objects;")
            .await;
        f.compare(&["system_view"]).await.unwrap();
    })
    .await;
}

#[tokio::test]
#[ignore]
async fn every_created_root_and_managed_overlap_is_accounted_for() {
    case("roots", |mut f| async move {
        f.both("CREATE TABLE dbo.t(a int);").await;
        let mut created = roots(&["t"]);
        created.insert(capture::ObjectIdentity { class: "sys.certificates".into(), name: vec!["unread".into()], signature: vec![] });
        refused(capture::compare_staged(&mut f.scratch, &mut f.target, &created, &[]).await, "unread");
        let mut managed = pbps_model::Schema::default();
        managed.tables.insert(pbps_model::TableName::new("dbo", "t"), pbps_model::Table::default());
        refused(capture::compare_staged(&mut f.scratch, &mut f.target, &roots(&["t"]), &[&managed]).await, "managed");
        f.both("CREATE USER hidden WITHOUT LOGIN; GRANT SELECT ON sys.sql_expression_dependencies TO hidden;").await;
        f.scratch.execute("EXECUTE AS USER=N'hidden';").await.unwrap();
        assert!(matches!(f.compare(&["t"]).await, Err(StageError::Capture(capture::CaptureError::Coverage(_)))));
    }).await;
}

fn cascading(
    action: pbps_model::ReferentialAction,
    update: bool,
    parents: &[&str],
) -> pbps_model::Schema {
    use pbps_model::*;
    let mut schema = Schema::default();
    let mut table = Table::default();
    for parent in parents {
        table.foreign_keys.insert(
            format!("to_{parent}").into(),
            ForeignKey {
                columns: vec!["a".into()],
                references_table: TableName::new("dbo", *parent),
                references_columns: vec!["a".into()],
                on_delete: if update {
                    ReferentialAction::NoAction
                } else {
                    action
                },
                on_update: if update {
                    action
                } else {
                    ReferentialAction::NoAction
                },
            },
        );
    }
    schema
        .tables
        .insert(TableName::new("dbo", "managed"), table);
    schema
}

async fn failure_number(conn: &mut Conn, sql: &str) -> String {
    let rows = conn.query(&format!("BEGIN TRY EXEC(N'{}'); SELECT N'accepted' AS code; END TRY BEGIN CATCH SELECT CONVERT(nvarchar(20),ERROR_NUMBER()) AS code; END CATCH;", sql.replace('\'', "''"))).await.unwrap();
    rows[0].try_get::<&str>("code").unwrap().unwrap().into()
}

#[tokio::test]
#[ignore]
async fn external_cascade_closure_preserves_multiple_path_refusal() {
    case("cascade", |mut f| async move {
        f.both("CREATE TABLE dbo.a(a int CONSTRAINT a_pk PRIMARY KEY); CREATE TABLE dbo.b(a int CONSTRAINT b_pk PRIMARY KEY); CREATE TABLE dbo.c(a int CONSTRAINT c_pk PRIMARY KEY); CREATE TABLE dbo.e(a int CONSTRAINT e_pk PRIMARY KEY);").await;
        f.target.execute("ALTER TABLE dbo.b ADD CONSTRAINT ba FOREIGN KEY(a) REFERENCES dbo.a(a) ON DELETE CASCADE; ALTER TABLE dbo.c ADD CONSTRAINT cb FOREIGN KEY(a) REFERENCES dbo.b(a) ON DELETE CASCADE; ALTER TABLE dbo.e ADD CONSTRAINT ea FOREIGN KEY(a) REFERENCES dbo.a(a) ON DELETE CASCADE;").await.unwrap();
        let managed = cascading(pbps_model::ReferentialAction::Cascade, false, &["a","c"]);
        let scope = capture::cascade_scope(&mut f.target, &[&managed]).await.unwrap();
        assert!(scope.retained.contains(&capture::object("dbo", "ba")));
        assert!(scope.retained.contains(&capture::object("dbo", "cb")));
        assert!(scope.retained.contains(&capture::object("dbo", "ea")));
        // Missing external-to-external paths refuse before a renderer can certify them.
        refused(capture::compare_staged(&mut f.scratch, &mut f.target, &roots(&["a","b","c","e"]), &[&managed]).await, "ba");
        f.scratch.execute("ALTER TABLE dbo.b ADD CONSTRAINT ba FOREIGN KEY(a) REFERENCES dbo.a(a) ON DELETE CASCADE; ALTER TABLE dbo.c ADD CONSTRAINT cb FOREIGN KEY(a) REFERENCES dbo.b(a) ON DELETE CASCADE; ALTER TABLE dbo.e ADD CONSTRAINT ea FOREIGN KEY(a) REFERENCES dbo.a(a) ON DELETE CASCADE;").await.unwrap();
        capture::compare_staged(&mut f.scratch, &mut f.target, &roots(&["a","b","c","e"]), &[&managed]).await.unwrap();
        // a -> b -> c and a -> c is a genuine second path, judged by SQL Server.
        let ddl = "CREATE TABLE dbo.managed(a int,CONSTRAINT to_a FOREIGN KEY(a) REFERENCES dbo.a(a) ON DELETE CASCADE,CONSTRAINT to_c FOREIGN KEY(a) REFERENCES dbo.c(a) ON DELETE CASCADE);";
        // SQL Server emits causal 1785 followed by 1750; TRY/CATCH and the
        // current driver retain the latter. Raw sqlcmd measurement pins both.
        assert_eq!(failure_number(&mut f.target,ddl).await, "1750");
        assert_eq!(failure_number(&mut f.scratch,ddl).await, "1750");
    }).await;
}

#[tokio::test]
#[ignore]
async fn noncascading_or_disjoint_actions_do_not_add_a_barrier() {
    case("actions", |mut f| async move {
        f.both("CREATE TABLE dbo.a(a int CONSTRAINT a_pk PRIMARY KEY); CREATE TABLE dbo.b(a int CONSTRAINT b_pk PRIMARY KEY,x int NULL DEFAULT 0); CREATE TABLE dbo.disjoint(a int CONSTRAINT disjoint_pk PRIMARY KEY); CREATE TABLE dbo.other(a int CONSTRAINT other_pk PRIMARY KEY);").await;
        let managed = cascading(pbps_model::ReferentialAction::Cascade, false, &["a","b"]);
        for (event, action) in [("DELETE","NO ACTION"),("DELETE","SET NULL"),("DELETE","SET DEFAULT"),("UPDATE","NO ACTION"),("UPDATE","SET NULL"),("UPDATE","SET DEFAULT"),("UPDATE","CASCADE")] {
            // Noncascading DELETE edges and disjoint UPDATE edges add no barrier.
            f.target.execute(&format!("ALTER TABLE dbo.b ADD CONSTRAINT ba FOREIGN KEY(x) REFERENCES dbo.a(a) ON {event} {action};")).await.unwrap();
            let scope = capture::cascade_scope(&mut f.target, &[&managed]).await.unwrap();
            assert!(!scope.retained.contains(&capture::object("dbo", "ba")));
            capture::compare_staged(&mut f.scratch, &mut f.target, &roots(&["a","b"]), &[&managed]).await.unwrap();
            let ddl = "CREATE TABLE dbo.managed(a int,CONSTRAINT to_a FOREIGN KEY(a) REFERENCES dbo.a(a) ON DELETE CASCADE,CONSTRAINT to_b FOREIGN KEY(a) REFERENCES dbo.b(a) ON DELETE CASCADE);";
            assert_eq!(failure_number(&mut f.target,ddl).await,"accepted");
            assert_eq!(failure_number(&mut f.scratch,ddl).await,"accepted");
            f.both("DROP TABLE dbo.managed;").await;
            f.target.execute("ALTER TABLE dbo.b DROP CONSTRAINT ba;").await.unwrap();
        }
        f.target.execute("ALTER TABLE dbo.other ADD CONSTRAINT unrelated FOREIGN KEY(a) REFERENCES dbo.disjoint(a) ON DELETE CASCADE;").await.unwrap();
        let scope = capture::cascade_scope(&mut f.target, &[&managed]).await.unwrap();
        assert!(!scope.retained.contains(&capture::object("dbo", "unrelated")));
    }).await;
}

#[tokio::test]
#[ignore]
async fn referencing_instead_of_trigger_blocks_matching_cascade() {
    case("trigger", |mut f| async move {
        f.both("CREATE TABLE dbo.a(a int CONSTRAINT a_pk PRIMARY KEY); CREATE TABLE dbo.managed(a int CONSTRAINT managed_pk PRIMARY KEY);").await;
        f.target.execute("CREATE TRIGGER dbo.barrier ON dbo.managed INSTEAD OF DELETE AS RETURN;").await.unwrap();
        let managed = cascading(pbps_model::ReferentialAction::Cascade, false, &["a"]);
        let scope = capture::cascade_scope(&mut f.target, &[&managed]).await.unwrap();
        assert!(scope.retained.contains(&capture::object("dbo", "barrier")));
        refused(capture::compare_staged(&mut f.scratch, &mut f.target, &roots(&["a"]), &[&managed]).await, "barrier");
        f.scratch.execute("CREATE TRIGGER dbo.barrier ON dbo.managed INSTEAD OF DELETE AS RETURN;").await.unwrap();
        capture::compare_staged(&mut f.scratch, &mut f.target, &roots(&["a"]), &[&managed]).await.unwrap();
        let ddl = "ALTER TABLE dbo.managed ADD CONSTRAINT ma FOREIGN KEY(a) REFERENCES dbo.a(a) ON DELETE CASCADE;";
        // The causal 1787 is followed by the reported 1750, as for 1785.
        assert_eq!(failure_number(&mut f.target,ddl).await, "1750");
        assert_eq!(failure_number(&mut f.scratch,ddl).await, "1750");
    }).await;
}

#[tokio::test]
#[ignore]
async fn parent_or_other_action_trigger_does_not_block() {
    case("othertrigger", |mut f| async move {
        f.both("CREATE TABLE dbo.a(a int CONSTRAINT a_pk PRIMARY KEY); CREATE TABLE dbo.managed(a int CONSTRAINT managed_pk PRIMARY KEY);").await;
        f.target.execute("CREATE TRIGGER dbo.parent_tr ON dbo.a INSTEAD OF DELETE AS RETURN;").await.unwrap();
        f.target.execute("CREATE TRIGGER dbo.other_tr ON dbo.managed INSTEAD OF UPDATE AS RETURN;").await.unwrap();
        let managed = cascading(pbps_model::ReferentialAction::Cascade, false, &["a"]);
        let scope = capture::cascade_scope(&mut f.target, &[&managed]).await.unwrap();
        assert!(!scope.retained.contains(&capture::object("dbo", "parent_tr")));
        assert!(!scope.retained.contains(&capture::object("dbo", "other_tr")));
        capture::compare_staged(&mut f.scratch, &mut f.target, &roots(&["a"]), &[&managed]).await.unwrap();
        let ddl = "ALTER TABLE dbo.managed ADD CONSTRAINT ma FOREIGN KEY(a) REFERENCES dbo.a(a) ON DELETE CASCADE;";
        assert_eq!(failure_number(&mut f.target,ddl).await, "accepted");
        assert_eq!(failure_number(&mut f.scratch,ddl).await, "accepted");
    }).await;
}

#[tokio::test]
#[ignore]
async fn view_and_table_function_result_columns_are_compared() {
    case("results", |mut f| async move {
        f.both("CREATE VIEW dbo.v AS SELECT CONVERT(int,NULL) AS a;")
            .await;
        f.compare(&["v"]).await.unwrap();
        f.scratch
            .execute("ALTER VIEW dbo.v AS SELECT ISNULL(CONVERT(int,NULL),0) AS a;")
            .await
            .unwrap();
        refused(f.compare(&["v"]).await, "a");
        f.both("CREATE FUNCTION dbo.f() RETURNS TABLE AS RETURN SELECT CONVERT(int,NULL) AS a;")
            .await;
        f.compare(&["f"]).await.unwrap();
        f.scratch
            .execute(
                "ALTER FUNCTION dbo.f() RETURNS TABLE AS RETURN SELECT CONVERT(bigint,NULL) AS a;",
            )
            .await
            .unwrap();
        refused(f.compare(&["f"]).await, "a");
    })
    .await;
}

#[tokio::test]
#[ignore]
async fn owner_names_can_differ_but_equality_with_managed_schemas_cannot() {
    case("owners", |mut f| async move {
        f.target
            .execute("CREATE USER target_owner WITHOUT LOGIN;")
            .await
            .unwrap();
        f.target
            .execute("CREATE SCHEMA ext AUTHORIZATION target_owner;")
            .await
            .unwrap();
        f.target
            .execute("CREATE SCHEMA app AUTHORIZATION target_owner;")
            .await
            .unwrap();
        f.scratch
            .execute("CREATE USER file_owner WITHOUT LOGIN;")
            .await
            .unwrap();
        f.scratch
            .execute("CREATE SCHEMA ext AUTHORIZATION file_owner;")
            .await
            .unwrap();
        f.scratch
            .execute("CREATE SCHEMA app AUTHORIZATION file_owner;")
            .await
            .unwrap();
        f.both("CREATE TABLE ext.t(a int);").await;
        let mut managed = pbps_model::Schema::default();
        managed.tables.insert(
            pbps_model::TableName::new("app", "managed"),
            pbps_model::Table::default(),
        );
        let created = BTreeSet::from([capture::object("ext", "t")]);
        capture::compare_staged(&mut f.scratch, &mut f.target, &created, &[&managed])
            .await
            .unwrap();
        f.scratch
            .execute("ALTER AUTHORIZATION ON SCHEMA::app TO dbo;")
            .await
            .unwrap();
        refused(
            capture::compare_staged(&mut f.scratch, &mut f.target, &created, &[&managed]).await,
            "owner_group",
        );
    })
    .await;
}

#[tokio::test]
#[ignore]
async fn type_defaults_are_skipped_and_type_owner_equality_is_preserved() {
    case("types", |mut f| async move {
        f.both("CREATE USER type_owner WITHOUT LOGIN;").await;
        f.both("CREATE SCHEMA ext AUTHORIZATION type_owner;").await;
        f.both("CREATE TYPE ext.amount FROM decimal(9,2);").await;
        let created = BTreeSet::from([capture::ObjectIdentity {
            class: capture::class::TYPE.into(),
            name: vec!["ext".into(), "amount".into()],
            signature: vec![],
        }]);
        capture::compare_staged(&mut f.scratch, &mut f.target, &created, &[])
            .await
            .unwrap();
        f.target
            .execute("ALTER AUTHORIZATION ON TYPE::ext.amount TO dbo;")
            .await
            .unwrap();
        refused(
            capture::compare_staged(&mut f.scratch, &mut f.target, &created, &[]).await,
            "owner_group",
        );
        f.target
            .execute("CREATE TYPE ext.rows_type AS TABLE(a int DEFAULT 1);")
            .await
            .unwrap();
        f.scratch
            .execute("CREATE TYPE ext.rows_type AS TABLE(a int);")
            .await
            .unwrap();
        let created = BTreeSet::from([capture::ObjectIdentity {
            class: capture::class::TYPE.into(),
            name: vec!["ext".into(), "rows_type".into()],
            signature: vec![],
        }]);
        capture::compare_staged(&mut f.scratch, &mut f.target, &created, &[])
            .await
            .unwrap();
    })
    .await;
}

fn spelling_refused(result: Result<capture::Staged, StageError>, target: &str, scratch: &str) {
    let Err(StageError::Refused(findings)) = result else {
        panic!("expected a named spelling refusal: {result:?}");
    };
    assert!(
        findings
            .iter()
            .any(|f| f.contains("the externals file spells")
                && f.contains(target)
                && f.contains(scratch)),
        "{findings:?}"
    );
    assert!(
        !findings
            .iter()
            .any(|f| f.contains("does not have") || f.contains(" lacks ")),
        "{findings:?}"
    );
}

#[tokio::test]
#[ignore]
async fn resolved_object_type_and_schema_spellings_refuse_by_both_names() {
    for (tag, collation, class, a, b, target_sql, scratch_sql, restore_sql) in [
        (
            "ci_name",
            "Latin1_General_100_CI_AS",
            capture::class::OBJECT,
            "T",
            "t",
            "CREATE TABLE dbo.T(a int);",
            "CREATE TABLE dbo.t(a int);",
            "DROP TABLE dbo.t; CREATE TABLE dbo.T(a int);",
        ),
        (
            "ai_name",
            "Latin1_General_100_CI_AI",
            capture::class::OBJECT,
            "Café",
            "cafe",
            "CREATE TABLE dbo.[Café](a int);",
            "CREATE TABLE dbo.cafe(a int);",
            "DROP TABLE dbo.cafe; CREATE TABLE dbo.[Café](a int);",
        ),
        (
            "type_name",
            "Latin1_General_100_CI_AS",
            capture::class::TYPE,
            "T",
            "t",
            "CREATE TYPE dbo.T FROM int;",
            "CREATE TYPE dbo.t FROM int;",
            "DROP TYPE dbo.t; CREATE TYPE dbo.T FROM int;",
        ),
        (
            "schema_name",
            "Latin1_General_100_CI_AS",
            capture::class::SCHEMA,
            "Ext",
            "ext",
            "CREATE SCHEMA Ext;",
            "CREATE SCHEMA ext;",
            "DROP SCHEMA ext; EXEC(N'CREATE SCHEMA Ext;');",
        ),
    ] {
        case(tag, move |mut f| async move {
            f.both(&format!("ALTER DATABASE CURRENT COLLATE {collation};"))
                .await;
            f.target.execute(target_sql).await.unwrap();
            f.scratch.execute(scratch_sql).await.unwrap();
            let root = |name: &str| capture::ObjectIdentity {
                class: class.into(),
                name: if class == capture::class::SCHEMA {
                    vec![name.into()]
                } else {
                    vec!["dbo".into(), name.into()]
                },
                signature: vec![],
            };
            spelling_refused(
                capture::compare_staged(
                    &mut f.scratch,
                    &mut f.target,
                    &BTreeSet::from([root(b)]),
                    &[],
                )
                .await,
                a,
                b,
            );
            f.scratch.execute(restore_sql).await.unwrap();
            capture::compare_staged(
                &mut f.scratch,
                &mut f.target,
                &BTreeSet::from([root(a)]),
                &[],
            )
            .await
            .unwrap();
            if class == capture::class::OBJECT {
                f.scratch
                    .execute(&format!("ALTER TABLE dbo.[{a}] ALTER COLUMN a bigint;"))
                    .await
                    .unwrap();
                refused(
                    capture::compare_staged(
                        &mut f.scratch,
                        &mut f.target,
                        &BTreeSet::from([root(a)]),
                        &[],
                    )
                    .await,
                    "a",
                );
            }
        })
        .await;
    }
}

#[tokio::test]
#[ignore]
async fn case_sensitive_root_difference_remains_named_absence() {
    case("cs_name", |mut f| async move {
        f.both("ALTER DATABASE CURRENT COLLATE Latin1_General_100_CS_AS;")
            .await;
        f.target
            .execute("CREATE TABLE dbo.T(a int);")
            .await
            .unwrap();
        f.scratch
            .execute("CREATE TABLE dbo.t(a int);")
            .await
            .unwrap();
        let Err(StageError::Refused(findings)) = f.compare(&["t"]).await else {
            panic!("case-sensitive absence must refuse");
        };
        assert!(
            findings
                .iter()
                .any(|f| f.contains("the target does not have") && f.contains("\"t\"")),
            "{findings:?}"
        );
        assert!(
            !findings.iter().any(|f| f.contains("spells")),
            "{findings:?}"
        );
        f.scratch
            .execute("DROP TABLE dbo.t; CREATE TABLE dbo.T(a int);")
            .await
            .unwrap();
        f.compare(&["T"]).await.unwrap();
    })
    .await;
}

#[tokio::test]
#[ignore]
async fn column_index_and_constraint_spellings_are_compared() {
    for (tag, target_sql, scratch_sql, class, root, a, b) in [
        (
            "column_name",
            "CREATE TABLE dbo.t(C int);",
            "CREATE TABLE dbo.t(c int);",
            capture::class::OBJECT,
            "t",
            "\"C\"",
            "\"c\"",
        ),
        (
            "index_name",
            "CREATE TABLE dbo.t(a int); CREATE UNIQUE INDEX IX ON dbo.t(a);",
            "CREATE TABLE dbo.t(a int); CREATE UNIQUE INDEX ix ON dbo.t(a);",
            capture::class::OBJECT,
            "t",
            "\"IX\"",
            "\"ix\"",
        ),
        (
            "constraint_name",
            "CREATE TABLE dbo.t(a int CONSTRAINT PK PRIMARY KEY);",
            "CREATE TABLE dbo.t(a int CONSTRAINT pk PRIMARY KEY);",
            capture::class::OBJECT,
            "t",
            "\"PK\"",
            "\"pk\"",
        ),
        (
            "type_column_name",
            "CREATE TYPE dbo.tt AS TABLE(C int);",
            "CREATE TYPE dbo.tt AS TABLE(c int);",
            capture::class::TYPE,
            "tt",
            "\"C\"",
            "\"c\"",
        ),
    ] {
        case(tag, move |mut f| async move {
            f.both("ALTER DATABASE CURRENT COLLATE Latin1_General_100_CI_AS;")
                .await;
            f.target.execute(target_sql).await.unwrap();
            f.scratch.execute(scratch_sql).await.unwrap();
            let created = BTreeSet::from([capture::ObjectIdentity {
                class: class.into(),
                name: vec!["dbo".into(), root.into()],
                signature: vec![],
            }]);
            spelling_refused(
                capture::compare_staged(&mut f.scratch, &mut f.target, &created, &[]).await,
                a,
                b,
            );
        })
        .await;
    }
}

#[tokio::test]
#[ignore]
async fn managed_aliases_are_rejected_as_overlap() {
    case("managed_name", |mut f| async move {
        f.both("ALTER DATABASE CURRENT COLLATE Latin1_General_100_CI_AS;")
            .await;
        f.both("CREATE SCHEMA Ext;").await;
        f.both("CREATE TABLE Ext.t(a int CONSTRAINT t_pk PRIMARY KEY);")
            .await;
        f.both("CREATE VIEW Ext.v AS SELECT a FROM Ext.t;").await;
        let mut managed = pbps_model::Schema::default();
        managed.tables.insert(
            pbps_model::TableName::new("ext", "T"),
            pbps_model::Table::default(),
        );
        let actual = capture::object("Ext", "t");
        let Err(StageError::Refused(findings)) = capture::compare_staged(
            &mut f.scratch,
            &mut f.target,
            &BTreeSet::from([actual]),
            &[&managed],
        )
        .await
        else {
            panic!("managed alias must overlap");
        };
        assert!(
            findings.iter().any(|f| f.contains("managed set")),
            "{findings:?}"
        );
        managed.modules.insert(
            pbps_model::ModuleId::Named(pbps_model::ObjectName::new("ext", "V")),
            pbps_model::Module {
                kind: pbps_model::ModuleKind::View,
                description: None,
                definition: "SELECT a FROM Ext.t".into(),
                options: Default::default(),
            },
        );
        let Err(StageError::Refused(findings)) = capture::compare_staged(
            &mut f.scratch,
            &mut f.target,
            &BTreeSet::from([capture::object("Ext", "v")]),
            &[&managed],
        )
        .await
        else {
            panic!("managed module alias must overlap");
        };
        assert!(
            findings.iter().any(|f| f.contains("managed set")),
            "{findings:?}"
        );
        // Constraint aliases share the same object namespace as tables/modules.
        managed
            .tables
            .get_mut(&pbps_model::TableName::new("ext", "T"))
            .unwrap()
            .primary_key = Some(pbps_model::PrimaryKey {
            name: Some("T_PK".into()),
            columns: vec!["a".into()],
            storage_parameters: Default::default(),
        });
        let Err(StageError::Refused(findings)) = capture::compare_staged(
            &mut f.scratch,
            &mut f.target,
            &BTreeSet::from([capture::object("Ext", "t_pk")]),
            &[&managed],
        )
        .await
        else {
            panic!("managed constraint alias must overlap");
        };
        assert!(
            findings.iter().any(|f| f.contains("managed set")),
            "{findings:?}"
        );
    })
    .await;
}

#[tokio::test]
#[ignore]
async fn cascade_seeds_resolve_to_catalog_spelling() {
    for update in [false, true] {
        case(if update {"update_seed_name"} else {"delete_seed_name"}, move |mut f| async move {
            f.both("ALTER DATABASE CURRENT COLLATE Latin1_General_100_CI_AS;").await;
            f.both("CREATE TABLE dbo.A(a int CONSTRAINT a_pk PRIMARY KEY); CREATE TABLE dbo.B(a int CONSTRAINT b_pk PRIMARY KEY); CREATE TABLE dbo.Managed(a int);").await;
            let event = if update { "UPDATE" } else { "DELETE" };
            f.target.execute(&format!("ALTER TABLE dbo.B ADD CONSTRAINT ba FOREIGN KEY(a) REFERENCES dbo.A(a) ON {event} CASCADE;")).await.unwrap();
            f.target.execute(&format!("CREATE TRIGGER dbo.barrier ON dbo.Managed INSTEAD OF {event} AS RETURN;")).await.unwrap();
            let alias = cascading(pbps_model::ReferentialAction::Cascade, update, &["a"]);
            let mut canonical = cascading(pbps_model::ReferentialAction::Cascade, update, &["A"]);
            let table = canonical.tables.remove(&pbps_model::TableName::new("dbo", "managed")).unwrap();
            canonical.tables.insert(pbps_model::TableName::new("dbo", "Managed"), table);
            let a = capture::cascade_scope(&mut f.target, &[&alias]).await.unwrap();
            let b = capture::cascade_scope(&mut f.target, &[&canonical]).await.unwrap();
            assert_eq!(a, b, "YAML aliases must retain the same canonical closure");
            assert!(a.retained.contains(&capture::object("dbo", "ba")));
            assert!(a.retained.contains(&capture::object("dbo", "barrier")));
            assert!(a.retained.contains(&capture::object("dbo", "A")));
            assert!(!a.retained.contains(&capture::object("dbo", "a")));
            refused(capture::compare_staged(&mut f.scratch, &mut f.target, &roots(&["A","B"]), &[&alias]).await, "ba");
            f.scratch.execute(&format!("ALTER TABLE dbo.B ADD CONSTRAINT ba FOREIGN KEY(a) REFERENCES dbo.A(a) ON {event} CASCADE;")).await.unwrap();
            f.scratch.execute(&format!("CREATE TRIGGER dbo.barrier ON dbo.Managed INSTEAD OF {event} AS RETURN;")).await.unwrap();
            let accepted = capture::compare_staged(&mut f.scratch, &mut f.target, &roots(&["A","B"]), &[&alias]).await.unwrap();
            let current = capture::capture(&mut f.target, accepted.scope()).await.unwrap();
            assert!(accepted.changed_since(&current).is_empty());
            // A managed FK is excluded even when its declaration uses an alias.
            f.both("DROP TRIGGER dbo.barrier;").await;
            f.both(&format!("ALTER TABLE dbo.Managed ADD CONSTRAINT to_a FOREIGN KEY(a) REFERENCES dbo.A(a) ON {event} CASCADE;")).await;
            let mut named_alias = alias.clone();
            let table = named_alias.tables.get_mut(&pbps_model::TableName::new("dbo", "managed")).unwrap();
            let fk = table.foreign_keys.remove("to_a").unwrap();
            table.foreign_keys.insert("TO_A".into(), fk);
            let scope = capture::cascade_scope(&mut f.target, &[&named_alias]).await.unwrap();
            assert!(!scope.retained.contains(&capture::object("dbo", "to_a")));
            assert!(scope.retained.contains(&capture::object("dbo", "ba")));
        }).await;
    }
}

#[tokio::test]
#[ignore]
async fn managed_schema_aliases_preserve_owner_anchors() {
    case("schema_anchor_name", |mut f| async move {
        f.both("ALTER DATABASE CURRENT COLLATE Latin1_General_100_CI_AS;")
            .await;
        f.both("CREATE USER u WITHOUT LOGIN; CREATE USER other_owner WITHOUT LOGIN;")
            .await;
        f.both("CREATE SCHEMA ext AUTHORIZATION u;").await;
        f.both("CREATE SCHEMA app AUTHORIZATION u;").await;
        f.both("CREATE TABLE ext.t(a int);").await;
        let mut managed = pbps_model::Schema::default();
        managed.tables.insert(
            pbps_model::TableName::new("APP", "future"),
            pbps_model::Table::default(),
        );
        let created = BTreeSet::from([capture::object("ext", "t")]);
        capture::compare_staged(&mut f.scratch, &mut f.target, &created, &[&managed])
            .await
            .unwrap();
        f.scratch
            .execute("ALTER AUTHORIZATION ON SCHEMA::app TO other_owner;")
            .await
            .unwrap();
        refused(
            capture::compare_staged(&mut f.scratch, &mut f.target, &created, &[&managed]).await,
            "owner_group",
        );
    })
    .await;
}

#[tokio::test]
#[ignore]
async fn accepted_handoff_keeps_exact_target_spelling() {
    case("handoff_name", |mut f| async move {
        f.both("ALTER DATABASE CURRENT COLLATE Latin1_General_100_CI_AS;")
            .await;
        f.both("CREATE TABLE dbo.T(a int);").await;
        let accepted = f.compare(&["T"]).await.unwrap();
        assert!(accepted.compared().contains(&capture::object("dbo", "T")));
        let current = capture::capture(&mut f.target, accepted.scope())
            .await
            .unwrap();
        assert!(accepted.changed_since(&current).is_empty());
        f.target
            .execute("EXEC sys.sp_rename N'dbo.T', N't';")
            .await
            .unwrap();
        let current = capture::capture(&mut f.target, accepted.scope())
            .await
            .unwrap();
        assert!(!accepted.changed_since(&current).is_empty());
    })
    .await;
}
