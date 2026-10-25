//! Database deployer reproduction and dbo containment (DEC-2267.1).

use super::connect_live;
use pbps_db::{Conn, DbError};
use pbps_mssql::resolver::{authorization as auth, capture, environment, scratch};
use std::collections::BTreeSet;

struct Fixture {
    target: Conn,
    scratch: Conn,
    scratch_admin: Conn,
    scratch_name: String,
}

// The task body runs on a separate Tokio task so even an assertion panic
// drops its NO REVERT connections before the owner removes both databases.
async fn case<F, Fut>(tag: &str, run: F)
where
    F: FnOnce(Fixture) -> Fut,
    Fut: Future<Output = ()> + Send + 'static,
{
    let target_server = std::env::var("PBPS_TEST_DB").unwrap();
    let scratch_server = std::env::var("PBPS_TEST_MSSQL_SCRATCH_DB").unwrap();
    let name = format!("pbps2267_{tag}_{}", std::process::id());
    for server in [&target_server, &scratch_server] {
        super::create_database_apart(server, &name).await;
    }
    let target_url = format!("{target_server};Database={name}");
    let scratch_url = format!("{scratch_server};Database={name}");
    let mut fixture = Fixture {
        target: connect_live(&target_url).await.unwrap(),
        scratch: connect_live(&scratch_url).await.unwrap(),
        scratch_admin: connect_live(&scratch_url).await.unwrap(),
        scratch_name: name.clone(),
    };
    fixture
        .target
        .execute(
            "CREATE USER dep WITHOUT LOGIN; CREATE ROLE outer_role; CREATE ROLE inner_role; \
        ALTER ROLE outer_role ADD MEMBER inner_role; ALTER ROLE inner_role ADD MEMBER dep; \
        GRANT VIEW DEFINITION,CREATE TABLE,CREATE VIEW TO dep; GRANT SELECT ON sys.sql_expression_dependencies TO dep;",
        )
        .await
        .unwrap();
    fixture
        .target
        .execute("CREATE SCHEMA app AUTHORIZATION dbo;")
        .await
        .unwrap();
    fixture
        .target
        .execute(
            "CREATE TABLE app.ref(i int CONSTRAINT ref_key PRIMARY KEY,j int); GRANT ALTER ON SCHEMA::app TO dep;",
        )
        .await
        .unwrap();
    let result = tokio::spawn(run(fixture)).await;
    for server in [&target_server, &scratch_server] {
        connect_live(server).await.unwrap().execute(&format!("ALTER DATABASE [{name}] SET SINGLE_USER WITH ROLLBACK IMMEDIATE; DROP DATABASE [{name}];")).await.unwrap();
    }
    // Only the escape case creates this run's owner login and server role.
    connect_live(&scratch_server).await.unwrap().execute(&format!(
        "IF EXISTS(SELECT 1 FROM sys.server_principals WHERE name=N'{name}_role') DROP SERVER ROLE [{name}_role]; \
         IF SUSER_ID(N'{name}_owner') IS NOT NULL DROP LOGIN [{name}_owner];"
    )).await.unwrap();
    result.unwrap();
}

struct Prepared {
    context: auth::AuthorizationContext,
    facts: pbps_db::resolver::environment::CatalogFacts,
    map: auth::PrincipalMap,
    target_owners: auth::OwnerPartition,
    current_owners: auth::OwnerPartition,
    schemas: Vec<String>,
}

async fn schemas(conn: &mut Conn) -> Vec<String> {
    conn.query("SELECT name FROM sys.schemas ORDER BY name;")
        .await
        .unwrap()
        .iter()
        .map(|row| row.try_get::<&str>("name").unwrap().unwrap().to_owned())
        .collect()
}

async fn owners(
    conn: &mut Conn,
    context: &auth::AuthorizationContext,
    objects: &[(&str, &str)],
) -> auth::OwnerPartition {
    let scope = capture::CaptureScope {
        retained: objects
            .iter()
            .map(|(schema, name)| capture::object(schema, name))
            .collect(),
        candidates: BTreeSet::new(),
    };
    owners_in_scope(conn, context, &scope).await
}

async fn owners_in_scope(
    conn: &mut Conn,
    context: &auth::AuthorizationContext,
    scope: &capture::CaptureScope,
) -> auth::OwnerPartition {
    let captured = capture::capture(conn, scope).await.unwrap();
    auth::OwnerPartition::from_capture(&captured, context).unwrap()
}

async fn object_acl(conn: &mut Conn) -> Vec<String> {
    conn.query(
        "SELECT CONCAT(major_id,N':',permission_name,N':',state,N':',minor_id,N':',grantee_principal_id,N':',grantor_principal_id) AS permission \
         FROM sys.database_permissions WHERE class=1 AND major_id IN(OBJECT_ID(N'app.ref'),OBJECT_ID(N'other.owned')) \
         ORDER BY major_id,permission_name,state,minor_id,grantee_principal_id,grantor_principal_id;",
    )
    .await
    .unwrap()
    .iter()
    .map(|row| row.try_get::<&str>("permission").unwrap().unwrap().to_owned())
    .collect()
}

impl Fixture {
    async fn reassign_owner_to_acl_role(&mut self) -> (Prepared, String, String) {
        self.target
            .execute(
                "CREATE USER app_owner WITHOUT LOGIN; CREATE ROLE zz_acl_role; \
             GRANT SELECT ON OBJECT::app.ref TO zz_acl_role WITH GRANT OPTION; \
             GRANT SELECT ON OBJECT::app.ref(i) TO dep AS zz_acl_role;",
            )
            .await
            .unwrap();
        self.target
            .execute("CREATE SCHEMA other AUTHORIZATION app_owner;")
            .await
            .unwrap();
        self.target
            .execute("CREATE TABLE other.owned(i int);")
            .await
            .unwrap();
        let mut p = self
            .prepare(
                false,
                &[TABLE[0], "CREATE TABLE other.owned(i int);"],
                OWNER_ALIAS_OBJECTS,
            )
            .await;
        let rows = self
            .scratch
            .query(
                "SELECT USER_NAME(s.principal_id) AS old_owner, r.name AS acl_role \
             FROM sys.schemas s CROSS JOIN sys.database_permissions p \
             JOIN sys.database_principals r ON r.principal_id=p.grantee_principal_id \
             WHERE s.name=N'other' AND p.class=1 AND p.major_id=OBJECT_ID(N'app.ref') \
             AND p.minor_id=0 AND p.permission_name=N'SELECT' AND p.state=N'W' AND r.type=N'R';",
            )
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        let old_owner = rows[0]
            .try_get::<&str>("old_owner")
            .unwrap()
            .unwrap()
            .to_owned();
        let acl_role = rows[0]
            .try_get::<&str>("acl_role")
            .unwrap()
            .unwrap()
            .to_owned();
        assert_ne!(old_owner, acl_role);
        let acl = object_acl(&mut self.scratch).await;
        // Reassign every subject in app_owner's group. The existing role's
        // grantee and grantor rows on app.ref must keep their ACL identities.
        self.scratch
            .execute(&format!(
                "ALTER AUTHORIZATION ON SCHEMA::other TO [{acl_role}]; \
                 ALTER AUTHORIZATION ON OBJECT::other.owned TO [{acl_role}];",
            ))
            .await
            .unwrap();
        assert_eq!(object_acl(&mut self.scratch).await, acl);
        let mut local = p.context.clone();
        local.principal.effective = p.map.deployer(&p.context).unwrap();
        p.current_owners = owners(&mut self.scratch, &local, OWNER_ALIAS_OBJECTS).await;
        assert!(p.target_owners.verify(&p.current_owners).is_ok());
        (p, old_owner, acl_role)
    }

    async fn prepare(
        &mut self,
        dbo: bool,
        rendered: &[&str],
        objects: &[(&str, &str)],
    ) -> Prepared {
        let scope = capture::CaptureScope {
            retained: objects
                .iter()
                .map(|(schema, name)| capture::object(schema, name))
                .collect(),
            candidates: BTreeSet::new(),
        };
        self.prepare_scope(dbo, rendered, &scope).await
    }

    async fn prepare_scope(
        &mut self,
        dbo: bool,
        rendered: &[&str],
        scope: &capture::CaptureScope,
    ) -> Prepared {
        if !dbo {
            self.target
                .execute("EXECUTE AS USER=N'dep' WITH NO REVERT;")
                .await
                .unwrap();
        }
        let schemas = schemas(&mut self.target).await;
        let mut context = auth::read(&mut self.target, &schemas).await.unwrap();
        let facts = environment::read(&mut self.target, &environment::Scope { schemas: &schemas })
            .await
            .unwrap();
        let target_owners = owners_in_scope(&mut self.target, &context, scope).await;
        auth::read_staged_permissions(&mut self.target, &mut context, &target_owners)
            .await
            .unwrap();
        let canonical: serde_json::Value = serde_json::from_slice(&context.canonical()).unwrap();
        if !context.staged.is_empty() {
            assert!(canonical["staged"].is_array());
        }
        let map = auth::PrincipalMap::generate(&context, &[], "deployer2267");
        scratch::create_sandbox(&mut self.scratch, "sandbox2267")
            .await
            .unwrap();
        scratch::enter_sandbox(&mut self.scratch, "sandbox2267")
            .await
            .unwrap();
        // Any ALTER LOGIN here would fail in the entered sandbox.
        auth::reconstruct_database(&mut self.scratch, &map, &context)
            .await
            .unwrap();
        for sql in rendered {
            self.scratch.execute(sql).await.unwrap();
        }
        auth::reconstruct_owners(&mut self.scratch, &map, &target_owners)
            .await
            .unwrap();
        let mut local = context.clone();
        if !dbo {
            local.principal.effective = map.deployer(&context).unwrap();
        }
        let current_owners = owners_in_scope(&mut self.scratch, &local, scope).await;
        auth::replay_staged_permissions(
            &mut self.scratch,
            &map,
            &context,
            &target_owners,
            &current_owners,
        )
        .await
        .unwrap();
        Prepared {
            context,
            facts,
            map,
            target_owners,
            current_owners,
            schemas,
        }
    }

    async fn enter(&mut self, prepared: &Prepared) -> Result<auth::SandboxDeployer, DbError> {
        let databases = scratch::database_names(&mut self.scratch_admin).await?;
        auth::enter_sandbox_deployer(
            &mut self.scratch,
            &prepared.map,
            &prepared.context,
            &prepared.facts,
            &auth::SandboxScope {
                sandbox: "sandbox2267",
                scratch: &self.scratch_name,
                databases: &databases,
            },
        )
        .await
    }

    async fn verify(&mut self, p: &Prepared) -> Vec<String> {
        let differences = auth::verify_staged_authorization(
            &mut self.scratch,
            &p.map,
            &p.context,
            &p.schemas,
            &p.target_owners,
            &p.current_owners,
        )
        .await
        .unwrap();
        if !differences.is_empty() {
            eprintln!("authorization differences: {differences:?}");
        }
        differences
    }
}

const TABLE: &[&str] = &["CREATE TABLE app.ref(i int CONSTRAINT ref_key PRIMARY KEY,j int);"];
const OBJECTS: &[(&str, &str)] = &[("app", "ref")];
const OWNER_ALIAS_OBJECTS: &[(&str, &str)] = &[("app", "ref"), ("other", "owned")];

async fn error_code(conn: &mut Conn, sql: &str) -> String {
    conn.execute(sql)
        .await
        .unwrap_err()
        .server_error_code()
        .unwrap()
}

#[tokio::test]
#[ignore = "needs the serial SQL Server resolver fixtures"]
async fn staged_refusals_show_hostile_names_while_lookups_and_recorded_identities_stay_raw() {
    case("shown_names", |mut f| async move {
        let name = "na\n\u{1b}\u{202e}\\n\\u{202e}雪']";
        let quoted = name.replace(']', "]]");
        let ddl = format!("CREATE TABLE app.[{quoted}]([{quoted}] int);");
        f.target.execute(&ddl).await.unwrap();
        // prepare reads the target's permissions and reconstructs the scratch
        // owner: both OBJECT_ID lookups must succeed with the raw spelling.
        let p = f.prepare(true, &[&ddl], &[("app", name)]).await;
        let id = capture::object("app", name);
        assert_eq!(p.context.staged.keys().collect::<Vec<_>>(), [&id]);
        assert!(
            p.context.staged[&id]
                .permissions
                .contains(&(name.into(), "SELECT".into()))
        );
        let staged = p.target_owners.restricted_to(&BTreeSet::from([id.clone()]));
        f.scratch
            .execute(&format!("DROP TABLE app.[{quoted}];"))
            .await
            .unwrap();
        let expected =
            r"staged object [app].[na\n\u{1b}\u{202e}\\n\\u{202e}雪']]] is absent or unreadable";
        let error = auth::reconstruct_owners(&mut f.scratch, &p.map, &staged)
            .await
            .unwrap_err();
        let DbError::Refused(message) = error else {
            panic!("expected a missing staged object, got {error:?}")
        };
        assert_eq!(message, expected, "owner reconstruction refusal");
        let mut context = p.context.clone();
        let canonical = context.canonical();
        let error = auth::read_staged_permissions(&mut f.scratch, &mut context, &staged)
            .await
            .unwrap_err();
        let DbError::Refused(message) = error else {
            panic!("expected a missing staged object, got {error:?}")
        };
        assert_eq!(message, expected, "permission capture refusal");
        assert_eq!(context.canonical(), canonical);
        assert_eq!(context.staged.keys().collect::<Vec<_>>(), [&id]);
        assert_eq!(id.name, ["app", name]);
    })
    .await;
}

#[tokio::test]
#[ignore = "needs the serial SQL Server resolver fixtures"]
async fn sandbox_deployer_reproduces_database_context_without_revert() {
    case("context",|mut f| async move {
        f.target.execute("ALTER USER dep WITH DEFAULT_SCHEMA=app; DENY CREATE PROCEDURE TO outer_role; SET LANGUAGE Deutsch; SET DATEFIRST 3; SET DATEFORMAT ymd; SET ARITHABORT OFF; SET NUMERIC_ROUNDABORT ON; SET QUOTED_IDENTIFIER OFF;").await.unwrap();
        let p=f.prepare(false,TABLE,OBJECTS).await;
        let entered=f.enter(&p).await.unwrap();
        assert!(!entered.is_dbo());
        assert!(f.verify(&p).await.is_empty());
        f.scratch.execute("CREATE TABLE app.accepted(i int);").await.unwrap();
        assert_eq!(error_code(&mut f.scratch,"CREATE PROCEDURE app.refused AS SELECT 1;").await,"262");
        let mut wrong=p.facts.clone();
        wrong.settings.get_mut("datefirst").unwrap().value="9".into();
        assert!(auth::apply_session_settings(&mut f.scratch,&wrong).await.is_err());
        // A file-added database permission must not pass final settlement.
        let dep=entered.name().raw().to_owned();
        f.scratch_admin.execute(&format!("GRANT CREATE FUNCTION TO [{dep}];")).await.unwrap();
        assert!(f.verify(&p).await.contains(&"database:permissions".into()));
    }).await;
}

#[tokio::test]
#[ignore = "needs the serial SQL Server resolver fixtures"]
async fn deployer_cannot_return_to_sandbox_or_login() {
    case("return", |mut f| async move {
        let p = f.prepare(false, TABLE, OBJECTS).await;
        f.enter(&p).await.unwrap();
        assert_eq!(
            error_code(&mut f.scratch, "EXECUTE AS USER=N'sandbox2267';").await,
            "15517"
        );
        assert_eq!(
            error_code(&mut f.scratch, "EXECUTE AS LOGIN=N'sa';").await,
            "15406"
        );
        assert_eq!(error_code(&mut f.scratch, "REVERT;").await, "15196");
    })
    .await;
}

#[tokio::test]
#[ignore = "needs the serial SQL Server resolver fixtures"]
async fn object_references_grant_is_replayed() {
    for grantee in ["outer_role", "public"] {
        case(
            if grantee == "public" {
                "references_public"
            } else {
                "references_role"
            },
            move |mut f| async move {
                f.target
                    .execute(&format!(
                        "GRANT REFERENCES ON OBJECT::app.ref TO {grantee};"
                    ))
                    .await
                    .unwrap();
                let p = f.prepare(false, TABLE, OBJECTS).await;
                f.enter(&p).await.unwrap();
                assert!(f.verify(&p).await.is_empty());
                f.scratch
                    .execute("CREATE TABLE app.child(i int REFERENCES app.ref(i));")
                    .await
                    .unwrap();
            },
        )
        .await;
    }
}

#[tokio::test]
#[ignore = "needs the serial SQL Server resolver fixtures"]
async fn object_references_deny_is_replayed() {
    case("references_deny",|mut f| async move {
        f.target.execute("GRANT REFERENCES ON SCHEMA::app TO dep; DENY REFERENCES ON OBJECT::app.ref TO outer_role;").await.unwrap();
        let p=f.prepare(false,TABLE,OBJECTS).await;
        f.enter(&p).await.unwrap();
        assert!(f.verify(&p).await.is_empty());
        assert_eq!(error_code(&mut f.scratch,"CREATE TABLE app.child(i int REFERENCES app.ref(i));").await,"229");
    }).await;
}

fn alias_scope() -> capture::CaptureScope {
    capture::CaptureScope {
        retained: BTreeSet::from([capture::ObjectIdentity {
            class: capture::class::TYPE.into(),
            name: vec!["dbo".into(), "amount".into()],
            signature: vec![],
        }]),
        candidates: BTreeSet::new(),
    }
}

async fn render_alias(f: &mut Fixture) -> Vec<String> {
    let scope = alias_scope();
    let inputs = capture::capture(&mut f.target, &scope).await.unwrap();
    pbps_mssql::resolver::externals::render::render(&inputs, scope.retained.first().unwrap())
        .unwrap()
        .statements
}

#[tokio::test]
#[ignore = "needs the serial SQL Server resolver fixtures"]
async fn type_references_grants_reach_the_entered_compile_session() {
    for grantee in ["outer_role", "public"] {
        case(
            if grantee == "public" { "type_public" } else { "type_role" },
            move |mut f| async move {
                f.target.execute(&format!(
                    "CREATE USER type_owner WITHOUT LOGIN; CREATE USER type_grantor WITHOUT LOGIN; \
                     CREATE TYPE dbo.amount FROM decimal(9,2) NOT NULL; \
                     ALTER AUTHORIZATION ON TYPE::dbo.amount TO type_owner; \
                     GRANT REFERENCES ON TYPE::dbo.amount TO type_grantor WITH GRANT OPTION AS type_owner; \
                     GRANT REFERENCES ON TYPE::dbo.amount TO {grantee} AS type_grantor;"
                )).await.unwrap();
                let rendered = render_alias(&mut f).await;
                let ddl: Vec<_> = rendered.iter().map(String::as_str).collect();
                let p = f.prepare_scope(false, &ddl, &alias_scope()).await;
                let entered = f.enter(&p).await.unwrap();
                assert!(f.verify(&p).await.is_empty());
                for conn in [&mut f.target, &mut f.scratch] {
                    // Compile and execute the same managed-table DDL under each
                    // deployer; its only REFERENCES capability is type-level.
                    conn.execute("CREATE TABLE app.using_alias(value dbo.amount);").await.unwrap();
                    let rows = conn.query("SELECT user_type_id FROM sys.columns WHERE object_id=OBJECT_ID(N'app.using_alias') AND user_type_id=TYPE_ID(N'dbo.amount');").await.unwrap();
                    assert_eq!(rows.len(), 1);
                    let rows = conn.query("SELECT HAS_PERMS_BY_NAME(N'dbo.amount',N'TYPE',N'REFERENCES') AS type_refs,HAS_PERMS_BY_NAME(N'dbo',N'SCHEMA',N'REFERENCES') AS schema_refs;").await.unwrap();
                    assert_eq!(rows[0].try_get::<i32>("type_refs").unwrap(), Some(1));
                    assert_eq!(rows[0].try_get::<i32>("schema_refs").unwrap(), Some(0));
                }
                // A file-added type permission must be detected at settlement.
                let dep = entered.name().raw().to_owned();
                f.scratch_admin.execute(&format!("GRANT EXECUTE ON TYPE::dbo.amount TO [{dep}];")).await.unwrap();
                assert!(f.verify(&p).await.contains(&"staged:permissions".into()));
            },
        ).await;
    }
}

#[tokio::test]
#[ignore = "needs the serial SQL Server resolver fixtures"]
async fn type_references_deny_overrides_schema_grants_in_the_compile_session() {
    case("type_deny", |mut f| async move {
        f.target.execute(
            "CREATE USER type_owner WITHOUT LOGIN; CREATE TYPE dbo.amount FROM decimal(9,2) NOT NULL; \
             ALTER AUTHORIZATION ON TYPE::dbo.amount TO type_owner; \
             GRANT REFERENCES ON SCHEMA::dbo TO outer_role; \
             DENY REFERENCES ON TYPE::dbo.amount TO outer_role;"
        ).await.unwrap();
        let rendered = render_alias(&mut f).await;
        let ddl: Vec<_> = rendered.iter().map(String::as_str).collect();
        let p = f.prepare_scope(false, &ddl, &alias_scope()).await;
        f.enter(&p).await.unwrap();
        assert!(f.verify(&p).await.is_empty());
        for conn in [&mut f.target, &mut f.scratch] {
            assert_eq!(error_code(conn,"CREATE TABLE app.denied_alias(value dbo.amount);").await,"15247");
            let rows = conn.query("SELECT HAS_PERMS_BY_NAME(N'dbo.amount',N'TYPE',N'REFERENCES') AS type_refs,HAS_PERMS_BY_NAME(N'dbo',N'SCHEMA',N'REFERENCES') AS schema_refs;").await.unwrap();
            assert_eq!(rows[0].try_get::<i32>("type_refs").unwrap(), Some(0));
            assert_eq!(rows[0].try_get::<i32>("schema_refs").unwrap(), Some(1));
            let rows = conn.query("SELECT OBJECT_ID(N'app.denied_alias') AS id;").await.unwrap();
            assert!(rows[0].try_get::<i32>("id").unwrap().is_none());
        }
    }).await;
}

#[tokio::test]
#[ignore = "needs the serial SQL Server resolver fixtures"]
async fn nested_role_denies_and_column_exceptions_are_replayed() {
    case("nested",|mut f| async move {
        f.target.execute("DENY SELECT ON OBJECT::app.ref TO outer_role; GRANT SELECT ON OBJECT::app.ref(i) TO outer_role; GRANT UPDATE ON OBJECT::app.ref TO inner_role; DENY UPDATE ON OBJECT::app.ref(j) TO outer_role;").await.unwrap();
        let p=f.prepare(false,TABLE,OBJECTS).await;
        f.enter(&p).await.unwrap();
        assert!(f.verify(&p).await.is_empty());
        f.scratch.execute("SELECT i FROM app.ref;").await.unwrap();
        assert_eq!(error_code(&mut f.scratch,"SELECT j FROM app.ref;").await,"230");
        f.scratch.execute("UPDATE app.ref SET i=1;").await.unwrap();
        assert_eq!(error_code(&mut f.scratch,"UPDATE app.ref SET j=1;").await,"230");
    }).await;
}

#[tokio::test]
#[ignore = "needs the serial SQL Server resolver fixtures"]
async fn object_grant_option_covers_column_grant_chains_without_extra_acl_rows() {
    case("object_column_chain", |mut f| async move {
        // A different upstream grantor keeps SQL Server from normalizing away
        // a redundant column option incorrectly synthesized under dbo.
        f.target
            .execute(
                "CREATE USER g WITHOUT LOGIN; CREATE USER h WITHOUT LOGIN; \
                 GRANT SELECT ON OBJECT::app.ref TO h WITH GRANT OPTION; \
                 GRANT SELECT ON OBJECT::app.ref TO g WITH GRANT OPTION AS h; \
                 GRANT SELECT ON OBJECT::app.ref(i) TO dep AS g; \
                 GRANT SELECT ON OBJECT::app.ref(j) TO outer_role AS g;",
            )
            .await
            .unwrap();
        let p = f.prepare(false, TABLE, OBJECTS).await;
        let rows = f
            .scratch_admin
            .query(
                "SELECT minor_id,state FROM sys.database_permissions \
             WHERE class=1 AND major_id=OBJECT_ID(N'app.ref');",
            )
            .await
            .unwrap();
        assert_eq!(rows.len(), 4, "replay must not add an ACL row");
        assert_eq!(
            rows.iter()
                .filter(|row| row.try_get::<&str>("state").unwrap() == Some("W"))
                .map(|row| row.try_get::<i32>("minor_id").unwrap().unwrap())
                .collect::<Vec<_>>(),
            [0, 0],
            "only the object-level grant options may remain",
        );
        f.enter(&p).await.unwrap();
        assert!(f.verify(&p).await.is_empty());
        f.scratch.execute("SELECT i,j FROM app.ref;").await.unwrap();
    })
    .await;
}

#[tokio::test]
#[ignore = "needs the serial SQL Server resolver fixtures"]
async fn column_grantors_without_captured_capabilities_get_only_the_needed_column_options() {
    case("column_chain_roots", |mut f| async move {
        f.target
            .execute("CREATE USER g WITHOUT LOGIN;")
            .await
            .unwrap();
        // No object or column grant option means the engine refuses the grant.
        assert_eq!(
            error_code(
                &mut f.target,
                "GRANT SELECT ON OBJECT::app.ref(i) TO dep AS g;"
            )
            .await,
            "15151",
        );
        f.target
            .execute(
                "GRANT SELECT ON OBJECT::app.ref(i,j) TO g WITH GRANT OPTION; \
                 GRANT SELECT ON OBJECT::app.ref(i,j) TO dep AS g;",
            )
            .await
            .unwrap();
        let mut p = f.prepare(false, TABLE, OBJECTS).await;
        // Supply only the downstream rows to exercise missing-capability
        // inference; clear the first replay so no previous ACL can enable it.
        p.context
            .staged
            .get_mut(&capture::object("app", "ref"))
            .unwrap()
            .grants
            .retain(|grant| grant.grantee.raw() == "dep");
        f.scratch
            .execute("ALTER AUTHORIZATION ON OBJECT::app.ref TO SCHEMA OWNER;")
            .await
            .unwrap();
        auth::replay_staged_permissions(
            &mut f.scratch,
            &p.map,
            &p.context,
            &p.target_owners,
            &p.current_owners,
        )
        .await
        .unwrap();
        let captured = &p.context.staged[&capture::object("app", "ref")].grants;
        assert_eq!(captured.len(), 2);
        assert!(captured.iter().all(|grant| matches!(
            grant.scope,
            auth::PermissionScope::Column {
                state: auth::ColumnPermissionState::Permission(auth::PermissionState::Grant),
                ..
            }
        )));
        // Neither pass has a captured grant option. Each must infer its own
        // column option, without borrowing the sibling's or widening to object.
        let rows = f
            .scratch_admin
            .query(
                "SELECT minor_id,state FROM sys.database_permissions \
             WHERE class=1 AND major_id=OBJECT_ID(N'app.ref');",
            )
            .await
            .unwrap();
        assert_eq!(rows.len(), 4);
        for column in [1, 2] {
            assert_eq!(
                rows.iter()
                    .filter(
                        |row| row.try_get::<i32>("minor_id").unwrap() == Some(column)
                            && row.try_get::<&str>("state").unwrap() == Some("W")
                    )
                    .count(),
                1
            );
        }
        assert!(
            rows.iter()
                .all(|row| row.try_get::<i32>("minor_id").unwrap() != Some(0))
        );
        f.enter(&p).await.unwrap();
        // The minimal inferred rows are visible to this deployer, so final
        // verification refuses rather than accepting extra captured authority.
        assert!(f.verify(&p).await.contains(&"staged:permissions".into()));
        f.scratch.execute("SELECT i,j FROM app.ref;").await.unwrap();
    })
    .await;
}

#[tokio::test]
#[ignore = "needs the serial SQL Server resolver fixtures"]
async fn column_revoke_is_preserved() {
    case("revoke",|mut f| async move {
        f.target.execute("GRANT SELECT ON OBJECT::app.ref TO outer_role; REVOKE SELECT ON OBJECT::app.ref(j) FROM outer_role;").await.unwrap();
        let p=f.prepare(false,TABLE,OBJECTS).await;
        let held=&p.context.staged[&capture::object("app","ref")].grants;
        assert!(held.iter().any(|g|matches!(&g.scope,auth::PermissionScope::Column { name,state:auth::ColumnPermissionState::Revoke } if name=="j")));
        f.enter(&p).await.unwrap();
        assert!(f.verify(&p).await.is_empty());
        f.scratch.execute("SELECT i FROM app.ref;").await.unwrap();
        assert_eq!(error_code(&mut f.scratch,"SELECT j FROM app.ref;").await,"230");
    }).await;
}

#[tokio::test]
#[ignore = "needs the serial SQL Server resolver fixtures"]
async fn dbo_deployer_ignores_object_deny_as_on_the_target() {
    case("dbo_deny", |mut f| async move {
        f.target
            .execute("DENY SELECT ON OBJECT::app.ref TO public;")
            .await
            .unwrap();
        f.target.execute("SELECT i FROM app.ref;").await.unwrap();
        let p = f.prepare(true, TABLE, OBJECTS).await;
        assert!(f.enter(&p).await.unwrap().is_dbo());
        assert!(f.verify(&p).await.is_empty());
        f.scratch.execute("SELECT i FROM app.ref;").await.unwrap();
    })
    .await;
}

#[tokio::test]
#[ignore = "needs the serial SQL Server resolver fixtures"]
async fn ordinary_deployer_obeys_the_same_object_deny() {
    case("ordinary_deny", |mut f| async move {
        // A db_owner member is still refused: it cannot stand in for dbo's
        // owner exemption when a staged object has a DENY.
        f.target
            .execute(
                "ALTER ROLE db_owner ADD MEMBER dep; DENY SELECT ON OBJECT::app.ref TO public;",
            )
            .await
            .unwrap();
        let p = f.prepare(false, TABLE, OBJECTS).await;
        assert_eq!(
            error_code(&mut f.target, "SELECT i FROM app.ref;").await,
            "229"
        );
        f.enter(&p).await.unwrap();
        assert!(f.verify(&p).await.is_empty());
        assert_eq!(
            error_code(&mut f.scratch, "SELECT i FROM app.ref;").await,
            "229"
        );
    })
    .await;
}

#[tokio::test]
#[ignore = "needs the serial SQL Server resolver fixtures"]
async fn trustworthy_scratch_refuses_before_dbo_entry() {
    case("trust", |mut f| async move {
        let p = f.prepare(true, TABLE, OBJECTS).await;
        f.scratch_admin
            .execute(&format!(
                "ALTER DATABASE [{}] SET TRUSTWORTHY ON;",
                f.scratch_name
            ))
            .await
            .unwrap();
        let outcome = f.enter(&p).await;
        // Restore before asserting, including on an unexpected success.
        f.scratch_admin
            .execute(&format!(
                "ALTER DATABASE [{}] SET TRUSTWORTHY OFF;",
                f.scratch_name
            ))
            .await
            .unwrap();
        let error = outcome.err().expect("TRUSTWORTHY ON must refuse");
        assert!(error.to_string().contains("TRUSTWORTHY"));
        let rows = f
            .scratch
            .query("SELECT USER_NAME() AS name;")
            .await
            .unwrap();
        assert_eq!(
            rows[0].try_get::<&str>("name").unwrap(),
            Some("sandbox2267")
        );
        assert!(f.enter(&p).await.is_ok());
    })
    .await;
}

#[tokio::test]
#[ignore = "needs the serial SQL Server resolver fixtures"]
async fn dbo_deployer_cannot_leave_scratch() {
    for ordinary_owner in [false, true] {
        case(if ordinary_owner { "escape_owner" } else { "escape_sa" }, move |mut f| async move {
        if ordinary_owner {
            let name=&f.scratch_name;
            f.scratch_admin.execute("USE master;").await.unwrap();
            f.scratch_admin.execute(&format!(
                "CREATE LOGIN [{name}_owner] WITH PASSWORD=N'Pbps!Owner2267',CHECK_POLICY=OFF; \
                 ALTER SERVER ROLE dbcreator ADD MEMBER [{name}_owner]; \
                 GRANT IMPERSONATE ON LOGIN::sa TO [{name}_owner]; \
                 CREATE SERVER ROLE [{name}_role] AUTHORIZATION [{name}_owner]; \
                 ALTER AUTHORIZATION ON DATABASE::[{name}] TO [{name}_owner];"
            )).await.unwrap();
            f.scratch_admin.execute(&format!("USE [{name}];")).await.unwrap();
        }
        let p = f.prepare(true, TABLE, OBJECTS).await;
        f.enter(&p).await.unwrap();
        for (sql, code) in [
            ("SELECT * FROM model.sys.objects;", "916"),
            ("SELECT * FROM msdb.dbo.sysjobs;", "229"),
            ("CREATE DATABASE pbps2267_forbidden_db;", "262"),
            (
                "CREATE LOGIN pbps2267_forbidden_login WITH PASSWORD=N'Pbps!NoEscape12345';",
                "15247",
            ),
            ("ALTER SERVER ROLE sysadmin ADD MEMBER sa;", "15151"),
            ("EXECUTE AS LOGIN=N'sa';", "15406"),
            (
                "SELECT * FROM OPENROWSET(BULK N'/etc/hostname',SINGLE_CLOB) AS content;",
                "4834",
            ),
            ("REVERT;", "15196"),
        ] {
            assert_eq!(error_code(&mut f.scratch, sql).await, code, "{sql}");
        }
        for option in ["TRUSTWORTHY", "DB_CHAINING"] {
            // Match the measurement's ERROR_NUMBER(): the server sends a
            // preceding permission error as well as final error 5069.
            let rows=f.scratch.query(&format!(
                "BEGIN TRY ALTER DATABASE CURRENT SET {option} ON; SELECT 0 AS code; \
                 END TRY BEGIN CATCH SELECT ERROR_NUMBER() AS code; END CATCH;"
            )).await.unwrap();
            assert_eq!(rows[0].try_get::<i32>("code").unwrap(),Some(5069));
        }
        if ordinary_owner {
            let name=&f.scratch_name;
            assert_eq!(error_code(&mut f.scratch,&format!("ALTER SERVER ROLE [{name}_role] ADD MEMBER [{name}_owner];")).await,"15151");
            assert_eq!(error_code(&mut f.scratch,&format!("EXECUTE AS LOGIN=N'{name}_owner';")).await,"15406");
        }
        // Match the original probe's batch boundary too. On Linux that
        // reports file access denial 4860 to the CATCH; a batch sent after
        // entry reports permission denial 4834 instead. LocalDB refuses this
        // batch with 4834 too, past the CATCH (measured on the Windows job).
        // Neither may return the file's bytes.
        let code=match f.scratch_admin.query(
            "EXECUTE AS USER=N'sandbox2267' WITH NO REVERT; EXECUTE AS USER=N'dbo' WITH NO REVERT; \
             DECLARE @content varchar(max); BEGIN TRY SELECT @content=BulkColumn FROM OPENROWSET(BULK '/etc/hostname',SINGLE_CLOB) b; SELECT 0 AS code; \
             END TRY BEGIN CATCH SELECT ERROR_NUMBER() AS code; END CATCH;"
        ).await {
            Ok(rows) => {
                assert_eq!(rows.len(),1);
                rows[0].try_get::<i32>("code").unwrap()
            }
            Err(error) => error.server_error_code().and_then(|code| code.parse().ok()),
        };
        assert!(matches!(code,Some(4860|4834)),"{code:?}");
    })
    .await;
    }
}

#[tokio::test]
#[ignore = "needs the serial SQL Server resolver fixtures"]
async fn owner_equality_across_schemas_is_preserved() {
    case("owner_equal",|mut f| async move {
        f.target.execute("CREATE USER shared_owner WITHOUT LOGIN; CREATE USER different_owner WITHOUT LOGIN;").await.unwrap();
        f.target.execute("CREATE SCHEMA other AUTHORIZATION shared_owner;").await.unwrap();
        f.target.execute("CREATE SCHEMA distinct_schema AUTHORIZATION different_owner;").await.unwrap();
        f.target.execute("ALTER AUTHORIZATION ON SCHEMA::app TO shared_owner; GRANT ALTER ON SCHEMA::app TO dep;").await.unwrap();
        f.target.execute("CREATE TABLE other.ref(i int);").await.unwrap();
        f.target.execute("CREATE TRIGGER app.tr ON app.ref AFTER INSERT AS SELECT 1;").await.unwrap();
        let p=f.prepare(false,&[TABLE[0],"CREATE TABLE other.ref(i int);","CREATE TRIGGER app.tr ON app.ref AFTER INSERT AS SELECT 1;"],&[("app","ref"),("other","ref"),("app","tr")]).await;
        assert!(p.target_owners.verify(&p.current_owners).is_ok());
        f.enter(&p).await.unwrap();
        assert!(f.verify(&p).await.is_empty());
    }).await;
}

#[tokio::test]
#[ignore = "needs the serial SQL Server resolver fixtures"]
async fn equivalent_owner_role_keeps_acl_principals_and_still_detects_extra_grants() {
    case("owner_acl_alias", |mut f| async move {
        let (p, old_owner, _) = f.reassign_owner_to_acl_role().await;
        f.enter(&p).await.unwrap();
        let differences = f.verify(&p).await;
        assert!(
            differences.is_empty(),
            "equivalent ownership must preserve ACL principal identities: {differences:?}"
        );
        f.scratch.execute("SELECT i FROM app.ref;").await.unwrap();
        // This changes the recorded ACL without changing the deployer's
        // effective permission answers.
        f.scratch_admin
            .execute(&format!(
                "GRANT UPDATE ON OBJECT::app.ref TO [{old_owner}];",
            ))
            .await
            .unwrap();
        assert_eq!(f.verify(&p).await, ["staged:permissions"]);
    })
    .await;
}

#[tokio::test]
#[ignore = "needs the serial SQL Server resolver fixtures"]
async fn non_equivalent_owner_role_refuses_with_matching_acl_rows() {
    case("owner_acl_partition", |mut f| async move {
        let (mut p, old_owner, _) = f.reassign_owner_to_acl_role().await;
        let acl = object_acl(&mut f.scratch).await;
        // Split the schema/object owner group with the recorded ACLs unchanged.
        f.scratch.execute(&format!(
            "ALTER AUTHORIZATION ON OBJECT::other.owned TO [{old_owner}];",
        )).await.unwrap();
        assert_eq!(object_acl(&mut f.scratch).await, acl);
        let mut local = p.context.clone();
        local.principal.effective = p.map.deployer(&p.context).unwrap();
        p.current_owners = owners(&mut f.scratch, &local, OWNER_ALIAS_OBJECTS).await;
        assert!(p.target_owners.verify(&p.current_owners).is_err());
        f.enter(&p).await.unwrap();
        let error = auth::verify_staged_authorization(
            &mut f.scratch, &p.map, &p.context, &p.schemas,
            &p.target_owners, &p.current_owners,
        ).await.unwrap_err();
        assert!(matches!(error, DbError::Refused(ref message) if message.contains("staged effective-owner equality differs")));
    })
    .await;
}

#[tokio::test]
#[ignore = "needs the serial SQL Server resolver fixtures"]
async fn different_effective_owner_refuses() {
    case("owner_refuse",|mut f| async move {
        let p=f.prepare(false,TABLE,OBJECTS).await;
        f.scratch.execute("CREATE USER file_owner WITHOUT LOGIN; ALTER AUTHORIZATION ON OBJECT::app.ref TO file_owner;").await.unwrap();
        let mut local=p.context.clone();
        local.principal.effective=p.map.deployer(&p.context).unwrap();
        let current=owners(&mut f.scratch,&local,OBJECTS).await;
        assert!(p.target_owners.verify(&current).is_err());
        assert!(auth::replay_staged_permissions(&mut f.scratch,&p.map,&p.context,&p.target_owners,&current).await.is_err());
        let rows=f.scratch.query("SELECT USER_NAME(principal_id) AS owner FROM sys.objects WHERE object_id=OBJECT_ID(N'app.ref');").await.unwrap();
        assert_eq!(rows[0].try_get::<&str>("owner").unwrap(),Some("file_owner"));
    }).await;
}

#[tokio::test]
#[ignore = "needs the serial SQL Server resolver fixtures"]
async fn ownership_is_replayed_before_object_acl() {
    case("owner_acl",|mut f| async move {
        f.target.execute("CREATE USER object_owner WITHOUT LOGIN; ALTER AUTHORIZATION ON OBJECT::app.ref TO object_owner; GRANT REFERENCES ON OBJECT::app.ref TO dep;").await.unwrap();
        let p=f.prepare(false,TABLE,OBJECTS).await;
        f.enter(&p).await.unwrap();
        assert!(f.verify(&p).await.is_empty());
        f.scratch.execute("CREATE TABLE app.child(i int REFERENCES app.ref(i));").await.unwrap();
        // Engine control: reassigning the same owner still clears the ACL.
        f.scratch_admin.execute("ALTER AUTHORIZATION ON OBJECT::app.ref TO SCHEMA OWNER;").await.unwrap();
        assert_eq!(error_code(&mut f.scratch,"CREATE TABLE app.second_child(i int REFERENCES app.ref(i));").await,"229");
    }).await;
}
