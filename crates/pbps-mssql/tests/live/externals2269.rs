//! File and renderer acceptance against two owned SQL Server fixtures.

use super::connect_live;
use pbps_db::Conn;
use pbps_mssql::resolver::{
    authorization as auth,
    capture::{self, StageError},
    externals, scratch,
};
use std::collections::BTreeSet;

struct Fixture {
    target: Conn,
    scratch: Conn,
    url: String,
}
const SANDBOX: &str = "sandbox2269";

async fn case<F, Fut>(tag: &str, run: F)
where
    F: FnOnce(Fixture) -> Fut,
    Fut: Future<Output = ()> + Send + 'static,
{
    let target = std::env::var("PBPS_TEST_DB").unwrap();
    let scratch_url = std::env::var("PBPS_TEST_MSSQL_SCRATCH_DB").unwrap();
    let name = format!("pbps2269_{tag}_{}", std::process::id());
    for server in [&target, &scratch_url] {
        connect_live(server)
            .await
            .unwrap()
            .execute(&format!("CREATE DATABASE [{name}];"))
            .await
            .unwrap();
    }
    let url = format!("{scratch_url};Database={name}");
    let mut scratch_conn = connect_live(&url).await.unwrap();
    scratch::create_sandbox(&mut scratch_conn, SANDBOX)
        .await
        .unwrap();
    scratch::enter_sandbox(&mut scratch_conn, SANDBOX)
        .await
        .unwrap();
    let f = Fixture {
        target: connect_live(&format!("{target};Database={name}"))
            .await
            .unwrap(),
        scratch: scratch_conn,
        url,
    };
    let result = tokio::spawn(run(f)).await;
    for server in [&target, &scratch_url] {
        connect_live(server).await.unwrap().execute(&format!("ALTER DATABASE [{name}] SET SINGLE_USER WITH ROLLBACK IMMEDIATE; DROP DATABASE [{name}];")).await.unwrap();
    }
    result.unwrap();
}

impl Fixture {
    async fn type_owners(
        &mut self,
        scope: &capture::CaptureScope,
    ) -> (
        auth::AuthorizationContext,
        auth::PrincipalMap,
        auth::OwnerPartition,
    ) {
        let captured = capture::capture(&mut self.target, scope).await.unwrap();
        let mut context = auth::read(&mut self.target, &["dbo".into()]).await.unwrap();
        let owners = auth::OwnerPartition::from_capture(&captured, &context).unwrap();
        auth::read_staged_permissions(&mut self.target, &mut context, &owners)
            .await
            .unwrap();
        let map = auth::PrincipalMap::generate(&context, &[], "externals2269");
        auth::reconstruct_database(&mut self.scratch, &map, &context)
            .await
            .unwrap();
        (context, map, owners)
    }

    async fn file(&mut self, sql: &str) -> Result<BTreeSet<capture::ObjectIdentity>, String> {
        let before = externals::inventory(&mut self.scratch, SANDBOX)
            .await
            .map_err(|e| e.to_string())?;
        let mut file = connect_live(&self.url).await.unwrap();
        scratch::enter_sandbox(&mut file, SANDBOX).await.unwrap();
        externals::run(&mut file, sql)
            .await
            .map_err(|e| e.to_string())?;
        drop(file);
        let after = externals::inventory(&mut self.scratch, SANDBOX)
            .await
            .map_err(|e| e.to_string())?;
        Ok(externals::created(&before, &after))
    }
    async fn compare(
        &mut self,
        roots: &BTreeSet<capture::ObjectIdentity>,
        managed: &[&pbps_model::Schema],
    ) -> Result<capture::Staged, StageError> {
        capture::compare_staged(&mut self.scratch, &mut self.target, roots, managed).await
    }
    async fn clean(&mut self) {
        scratch::inventory::cleanup(&mut self.scratch, Some(SANDBOX))
            .await
            .unwrap();
        assert!(
            scratch::inventory::read(&mut self.scratch, Some(SANDBOX))
                .await
                .unwrap()
                .is_empty()
        );
    }
}
fn roots(names: &[&str]) -> BTreeSet<capture::ObjectIdentity> {
    names.iter().map(|n| capture::object("dbo", n)).collect()
}
fn refused(result: Result<capture::Staged, StageError>, name: &str) {
    assert!(format!("{:?}", result.unwrap_err()).contains(name));
}

#[tokio::test]
#[ignore]
async fn external_file_managed_object_refuses() {
    case("managed", |mut f| async move {
        f.target
            .execute("CREATE TABLE dbo.t(i int);")
            .await
            .unwrap();
        let created = f.file("CREATE TABLE dbo.t(i int);").await.unwrap();
        let mut managed = pbps_model::Schema::default();
        managed.tables.insert(
            pbps_model::TableName::new("dbo", "t"),
            pbps_model::Table::default(),
        );
        refused(f.compare(&created, &[&managed]).await, "managed set");
        f.compare(&created, &[]).await.unwrap();
    })
    .await;
}
#[tokio::test]
#[ignore]
async fn external_file_object_absent_on_target_refuses() {
    case("absent", |mut f| async move {
        let created = f.file("CREATE TABLE dbo.only_file(i int);").await.unwrap();
        refused(f.compare(&created, &[]).await, "only_file");
        f.target
            .execute("CREATE TABLE dbo.only_file(i int);")
            .await
            .unwrap();
        f.compare(&created, &[]).await.unwrap();
    })
    .await;
}
#[tokio::test]
#[ignore]
async fn unreferenced_uncompared_file_object_refuses_by_name() {
    case("unreached",|mut f| async move {
        let created=f.file("CREATE SCHEMA unused AUTHORIZATION dbo;\nGO\nCREATE TABLE unused.t(i int); CREATE CERTIFICATE extra_certificate ENCRYPTION BY PASSWORD=N'Pbps!Certificate2269' WITH SUBJECT=N'fixture';").await.unwrap();
        refused(f.compare(&created,&[]).await,"extra_certificate");
        assert!(created.contains(&capture::object("unused","t")));
        f.target.execute("CREATE QUEUE dbo.uncovered_queue;").await.unwrap();
        let queue = f.file("CREATE QUEUE dbo.uncovered_queue;").await.unwrap();
        let error = format!("{:?}",f.compare(&queue,&[]).await.unwrap_err());
        assert!(error.contains("uncovered_queue") && error.contains("object kind"),"{error}");
    }).await;
}
#[tokio::test]
#[ignore]
async fn hidden_inventory_is_not_empty_inventory() {
    case("hidden",|mut f| async move {
        f.scratch.execute("CREATE TABLE dbo.secret(i int); CREATE USER blind WITHOUT LOGIN; EXECUTE AS USER=N'blind';").await.unwrap();
        let error=externals::inventory(&mut f.scratch,SANDBOX).await.unwrap_err();
        assert!(error.to_string().contains("owner") || error.to_string().contains("VIEW DEFINITION"),"{error}");
        f.scratch.execute("REVERT;").await.unwrap();
        assert!(!scratch::inventory::read(&mut f.scratch,Some(SANDBOX)).await.unwrap().is_empty());
        let mut admin = connect_live(&f.url).await.unwrap();
        admin.execute("DENY VIEW DEFINITION TO sandbox2269;").await.unwrap();
        assert!(externals::inventory(&mut f.scratch,SANDBOX).await.unwrap_err().to_string().contains("VIEW DEFINITION"));
        assert!(!scratch::inventory::read(&mut admin,Some(SANDBOX)).await.unwrap().is_empty());
    }).await;
}
#[tokio::test]
#[ignore]
async fn unreadable_external_shape_refuses() {
    case("encrypted", |mut f| async move {
        f.target
            .execute("CREATE PROCEDURE dbo.secret WITH ENCRYPTION AS SELECT 1;")
            .await
            .unwrap();
        let created = f
            .file("CREATE PROCEDURE dbo.secret WITH ENCRYPTION AS SELECT 1;")
            .await
            .unwrap();
        refused(f.compare(&created, &[]).await, "secret");
    })
    .await;
}
#[tokio::test]
#[ignore]
async fn rendered_external_matches_complete_compared_shape() {
    case("render",|mut f| async move {
        for (ddl,id) in [
            ("CREATE TYPE dbo.amount FROM decimal(9,2) NOT NULL;",capture::ObjectIdentity { class:capture::class::TYPE.into(),name:vec!["dbo".into(),"amount".into()],signature:vec![] }),
            ("CREATE TABLE dbo.t(i int IDENTITY(3,2) CONSTRAINT t_pk PRIMARY KEY,j nvarchar(20) NULL); CREATE UNIQUE INDEX t_unique ON dbo.t(j DESC) INCLUDE(i);",capture::object("dbo","t")),
            ("CREATE VIEW dbo.v AS SELECT CAST(NULL AS int) AS i;",capture::object("dbo","v")),
            ("CREATE SEQUENCE dbo.seq AS bigint START WITH 5 INCREMENT BY 2 MINVALUE 1 MAXVALUE 1000 CYCLE CACHE 7;",capture::object("dbo","seq")),
            ("CREATE FUNCTION dbo.fn(@i int) RETURNS int WITH SCHEMABINDING AS BEGIN RETURN @i+1; END;",capture::object("dbo","fn")),
        ] {
            f.target.execute(ddl).await.unwrap();
            let scope=capture::CaptureScope { retained:BTreeSet::from([id.clone()]),candidates:BTreeSet::new() };
            let inputs=capture::capture(&mut f.target,&scope).await.unwrap();
            let rendered=externals::render::render(&inputs,&id).unwrap_or_else(|e|panic!("{id:?}: {e:?}"));
            let before = externals::inventory(&mut f.scratch,SANDBOX).await.unwrap();
            for sql in rendered.statements { f.scratch.execute(&sql).await.unwrap_or_else(|e|panic!("{sql}: {e}")); }
            if id == capture::object("dbo","t") {
                f.scratch.execute("ALTER AUTHORIZATION ON OBJECT::dbo.t TO dbo;").await.unwrap();
            }
            let after = externals::inventory(&mut f.scratch,SANDBOX).await.unwrap();
            let created = externals::created(&before,&after);
            assert!(created.contains(&id));
            f.compare(&created,&[]).await.unwrap_or_else(|e|panic!("{id:?}: {e:?}"));
        }
        // A relevant external-to-external cascade is itself a rendered shape.
        f.target.execute("CREATE TABLE dbo.x(i int CONSTRAINT x_pk PRIMARY KEY); ALTER TABLE dbo.x ADD CONSTRAINT xt FOREIGN KEY(i) REFERENCES dbo.t(i) ON DELETE CASCADE;").await.unwrap();
        for id in [capture::object("dbo","x"),capture::object("dbo","xt")] {
            let scope=capture::CaptureScope { retained:BTreeSet::from([id.clone()]),candidates:BTreeSet::new() };
            let inputs=capture::capture(&mut f.target,&scope).await.unwrap();
            for sql in externals::render::render(&inputs,&id).unwrap().statements { f.scratch.execute(&sql).await.unwrap_or_else(|e|panic!("{sql}: {e}")); }
        }
        f.compare(&roots(&["t","x","xt"]),&[]).await.unwrap();
        f.scratch.execute("ALTER TABLE dbo.t ALTER COLUMN j nvarchar(30) NULL;").await.unwrap();
        refused(f.compare(&roots(&["t"]),&[]).await,"max_length");
        f.scratch.execute("ALTER SEQUENCE dbo.seq INCREMENT BY 3;").await.unwrap();
        refused(f.compare(&roots(&["seq"]),&[]).await,"seq");
    }).await;
}

fn alias(name: &str) -> capture::ObjectIdentity {
    capture::ObjectIdentity {
        class: capture::class::TYPE.into(),
        name: vec!["dbo".into(), name.into()],
        signature: vec![],
    }
}

#[tokio::test]
#[ignore]
async fn rendered_alias_type_explicit_and_inherited_owners_are_accepted() {
    case("type_owners", |mut f| async move {
        f.target
            .execute(
                "CREATE USER alias_owner WITHOUT LOGIN; \
                 CREATE TYPE dbo.explicit_alias FROM decimal(9,2) NOT NULL; \
                 CREATE TYPE dbo.inherited_alias FROM int NULL; \
                 ALTER AUTHORIZATION ON TYPE::dbo.explicit_alias TO alias_owner;",
            )
            .await
            .unwrap();
        let scope = capture::CaptureScope {
            retained: BTreeSet::from([alias("explicit_alias"), alias("inherited_alias")]),
            candidates: BTreeSet::new(),
        };
        let (context, map, owners) = f.type_owners(&scope).await;
        let inputs = capture::capture(&mut f.target, &scope).await.unwrap();
        let before = externals::inventory(&mut f.scratch, SANDBOX).await.unwrap();
        for id in &scope.retained {
            for sql in externals::render::render(&inputs, id).unwrap().statements {
                f.scratch.execute(&sql).await.unwrap();
            }
        }
        let after = externals::inventory(&mut f.scratch, SANDBOX).await.unwrap();
        let rendered = externals::created(&before, &after);
        assert_eq!(rendered, scope.retained);
        auth::reconstruct_owners(&mut f.scratch, &map, &owners.restricted_to(&rendered))
            .await
            .unwrap();
        let current = capture::capture(&mut f.scratch, &scope).await.unwrap();
        owners
            .verify(&auth::OwnerPartition::from_capture(&current, &context).unwrap())
            .unwrap();
        let rows = f.scratch.query("SELECT t.principal_id FROM sys.types t JOIN sys.schemas s ON s.schema_id=t.schema_id WHERE s.name=N'dbo' AND t.name=N'explicit_alias' AND t.principal_id<>s.principal_id;").await.unwrap();
        assert_eq!(rows.len(), 1, "the rendered alias needs its explicit owner");
        f.compare(&rendered, &[]).await.unwrap();
    })
    .await;
}

#[tokio::test]
#[ignore]
async fn file_alias_type_wrong_owner_is_never_repaired() {
    case("file_type_owner", |mut f| async move {
        f.target.execute("CREATE USER alias_owner WITHOUT LOGIN; CREATE TYPE dbo.file_alias FROM int NULL; CREATE TYPE dbo.rendered_alias FROM int NULL; ALTER AUTHORIZATION ON TYPE::dbo.file_alias TO alias_owner; ALTER AUTHORIZATION ON TYPE::dbo.rendered_alias TO alias_owner;").await.unwrap();
        let scope = capture::CaptureScope {
            retained: BTreeSet::from([alias("file_alias"), alias("rendered_alias")]),
            candidates: BTreeSet::new(),
        };
        let (context, map, owners) = f.type_owners(&scope).await;
        let from_file = f.file("CREATE TYPE dbo.file_alias FROM int NULL;").await.unwrap();
        assert_eq!(from_file, BTreeSet::from([alias("file_alias")]));
        let middle = externals::inventory(&mut f.scratch, SANDBOX).await.unwrap();
        let inputs = capture::capture(&mut f.target, &scope).await.unwrap();
        for sql in externals::render::render(&inputs, &alias("rendered_alias")).unwrap().statements {
            f.scratch.execute(&sql).await.unwrap();
        }
        let after = externals::inventory(&mut f.scratch, SANDBOX).await.unwrap();
        let rendered = externals::created(&middle, &after);
        assert_eq!(rendered, BTreeSet::from([alias("rendered_alias")]));
        auth::reconstruct_owners(&mut f.scratch, &map, &owners.restricted_to(&rendered))
            .await
            .unwrap();
        let rows = f.scratch.query("SELECT principal_id FROM sys.types WHERE user_type_id=TYPE_ID(N'dbo.file_alias');").await.unwrap();
        assert_eq!(rows.len(), 1);
        assert!(rows[0].try_get::<i32>("principal_id").unwrap().is_none());
        let current = capture::capture(&mut f.scratch, &scope).await.unwrap();
        let error = owners.verify(&auth::OwnerPartition::from_capture(&current, &context).unwrap()).unwrap_err();
        assert!(error.to_string().contains("file-supplied ownership is not repaired"));
        refused(f.compare(&scope.retained, &[]).await, "owner_group");
    })
    .await;
}
#[tokio::test]
#[ignore]
async fn unsupported_renderer_shape_refuses_by_name() {
    case("unsupported", |mut f| async move {
        f.target
            .execute("CREATE VIEW dbo.nonnull AS SELECT ISNULL(CONVERT(int,NULL),0) AS i;")
            .await
            .unwrap();
        let id = capture::object("dbo", "nonnull");
        let scope = capture::CaptureScope {
            retained: BTreeSet::from([id.clone()]),
            candidates: BTreeSet::new(),
        };
        let captured = capture::capture(&mut f.target, &scope).await.unwrap();
        let error = externals::render::render(&captured, &id).unwrap_err();
        assert!(format!("{error:?}").contains("nonnull"));
        assert!(format!("{error:?}").contains("non-nullable"));
        let created = f
            .file("CREATE VIEW dbo.nonnull AS SELECT CAST(NULL AS int) AS i;")
            .await
            .unwrap();
        refused(f.compare(&created, &[]).await, "nullable");
    })
    .await;
}
#[tokio::test]
#[ignore]
async fn failed_file_and_comparison_cleanup_owned_objects() {
    case("cleanup", |mut f| async move {
        assert!(
            f.file("CREATE TABLE dbo.partial(i int);\nGO\nTHROW 51001,'file failure',1;")
                .await
                .unwrap_err()
                .contains("file failure")
        );
        f.clean().await;
        let created = f.file("CREATE TABLE dbo.absent(i int);").await.unwrap();
        refused(f.compare(&created, &[]).await, "absent");
        f.clean().await;
    })
    .await;
}
#[tokio::test]
#[ignore]
async fn partial_commit_does_not_escape_owned_cleanup() {
    case("commit",|mut f| async move {
        assert!(f.file("BEGIN TRANSACTION; CREATE TABLE dbo.a(i int); EXEC(N'COMMIT; CREATE TABLE dbo.b(i int);'); THROW 51001,'after commit',1;").await.is_err());
        let names=scratch::inventory::read(&mut f.scratch,Some(SANDBOX)).await.unwrap().names();
        assert!(names.iter().any(|n|n.contains("dbo.a")) && names.iter().any(|n|n.contains("dbo.b")),"{names:?}");
        f.clean().await;
    }).await;
}

#[tokio::test]
#[ignore]
async fn cancelled_file_cleanup_owned_objects() {
    case("cancel", |mut f| async move {
        let mut file = connect_live(&f.url).await.unwrap();
        scratch::enter_sandbox(&mut file, SANDBOX).await.unwrap();
        let running = tokio::spawn(async move {
            externals::run(
                &mut file,
                "CREATE TABLE dbo.cancelled2269(i int); WAITFOR DELAY '00:00:30';",
            )
            .await
        });
        let mut observed = false;
        for _ in 0..200 {
            let rows = f
                .scratch
                .query("SELECT OBJECT_ID(N'dbo.cancelled2269') AS id;")
                .await
                .unwrap();
            if rows[0].try_get::<i32>("id").unwrap().is_some() {
                observed = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        running.abort();
        let cancelled = running.await;
        // The run keeps cleanup ownership while its dedicated file future
        // is cancelled, including DDL committed before the suspended batch.
        f.clean().await;
        assert!(observed);
        assert!(cancelled.is_err_and(|error| error.is_cancelled()));
    })
    .await;
}
#[tokio::test]
#[ignore]
async fn go_separator_split_respects_quotes_and_comments() {
    case("go",|mut f| async move {
        let created=f.file("CREATE TABLE dbo.[GO](i int);\nGO -- split\n/* GO\n/* nested */\nGO\n*/ INSERT dbo.[GO] VALUES(1); SELECT N'line\nGO\nstring';\ngO /* comment */\nCREATE VIEW dbo.v AS SELECT i FROM dbo.[GO];").await.unwrap();
        assert_eq!(created,roots(&["GO","v"]));
        assert_eq!(f.scratch.query("SELECT COUNT(*) AS n FROM dbo.[GO];").await.unwrap()[0].try_get::<i32>("n").unwrap(),Some(1));
    }).await;
}
#[tokio::test]
#[ignore]
async fn sqlcmd_directive_refuses() {
    case("sqlcmd", |mut f| async move {
        for command in [":r other.sql", ":setvar x y", "!! echo x", "GO 5"] {
            assert!(f.file(command).await.is_err());
            assert!(
                scratch::inventory::read(&mut f.scratch, Some(SANDBOX))
                    .await
                    .unwrap()
                    .is_empty()
            );
        }
    })
    .await;
}
#[tokio::test]
#[ignore]
async fn external_depending_on_managed_object_refuses_by_name() {
    case("chain",|mut f| async move {
        f.target.execute("CREATE TABLE dbo.a(i int CONSTRAINT a_pk PRIMARY KEY); CREATE TABLE dbo.managed(i int CONSTRAINT managed_pk PRIMARY KEY,CONSTRAINT to_a FOREIGN KEY(i) REFERENCES dbo.a(i) ON DELETE CASCADE); CREATE TABLE dbo.x(i int CONSTRAINT x_pk PRIMARY KEY); ALTER TABLE dbo.x ADD CONSTRAINT x_managed FOREIGN KEY(i) REFERENCES dbo.managed(i) ON DELETE CASCADE;").await.unwrap();
        let created=f.file("CREATE TABLE dbo.a(i int CONSTRAINT a_pk PRIMARY KEY); CREATE TABLE dbo.x(i int CONSTRAINT x_pk PRIMARY KEY);").await.unwrap();
        let mut managed=pbps_model::Schema::default();
        let mut table=pbps_model::Table::default();
        table.foreign_keys.insert("to_a".into(),pbps_model::ForeignKey {columns:vec!["i".into()],references_table:pbps_model::TableName::new("dbo","a"),references_columns:vec!["i".into()],on_delete:pbps_model::ReferentialAction::Cascade,on_update:pbps_model::ReferentialAction::NoAction});
        managed.tables.insert(pbps_model::TableName::new("dbo","managed"),table);
        let error=format!("{:?}",f.compare(&created,&[&managed]).await.unwrap_err());
        assert!(error.contains("x_managed") && error.contains("managed") && error.contains("#2238"),"{error}");
        let required=capture::required_externals(&mut f.target,&[&managed]).await.unwrap();
        let error=externals::render::missing(&required,&created).unwrap_err().join(";");
        assert!(error.contains("x_managed") && error.contains("#2238"),"{error}");
        let more=f.file("CREATE TABLE dbo.managed(i int CONSTRAINT managed_pk PRIMARY KEY,CONSTRAINT to_a FOREIGN KEY(i) REFERENCES dbo.a(i) ON DELETE CASCADE); ALTER TABLE dbo.x ADD CONSTRAINT x_managed FOREIGN KEY(i) REFERENCES dbo.managed(i) ON DELETE CASCADE;").await.unwrap();
        let all=created.union(&more).cloned().chain([capture::object("dbo","x_managed")]).collect();
        f.compare(&all,&[]).await.unwrap();
    }).await;
}
