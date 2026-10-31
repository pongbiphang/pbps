//! Credentials and targets can only be derived from this run's owned handle.
use super::revision::Assembled;
use crate::{db::Target, test_scratch::TestScratch};
use anyhow::{Context as _, ensure};
use pbps_db::{Conn, Driver};
use pbps_dialect::Dialect as _;
use pbps_model::{Schema, TableName};

pub(crate) struct Session<'a> {
    scratch: &'a TestScratch<'a>,
    target: Target,
    fixture: String,
    assertion: String,
    roles: [String; 3],
    key: std::path::PathBuf,
}
fn quoted(name: &str) -> anyhow::Result<String> {
    Ok(pbps_pg::Postgres::new().quote_ident(name)?)
}
fn table(name: &TableName) -> anyhow::Result<String> {
    Ok(format!("{}.{}", quoted(&name.schema)?, quoted(&name.name)?))
}
impl<'a> Session<'a> {
    pub async fn new(scratch: &'a TestScratch<'a>) -> anyhow::Result<Self> {
        let admin = scratch.connection();
        ensure!(
            !admin.contains("://"),
            "the owned adapter must return a keyword connection"
        );
        let mut conn = Conn::connect(Driver::Postgres, admin).await?;
        let rows = conn.query("SELECT current_user::text AS role").await?;
        let provision_role: pbps_model::RoleName = rows
            .first()
            .context("missing owned provision role result")?
            .try_get::<&str>("role")?
            .context("NULL owned provision role")?
            .into();
        // The frozen declarations must never capture an infrastructure identity.
        let roles = ["apply", "fixture", "assertion"]
            .map(|kind| format!("pbps_test_{kind}_{}", scratch.run_id()));
        let mut passwords = Vec::new();
        for role in &roles {
            let password = format!("{:032x}", rand::random::<u128>());
            let sql = format!(
                concat!(
                    "CREATE ROLE {role} LOGIN NOSUPERUSER NOCREATEDB ",
                    "NOCREATEROLE NOREPLICATION NOBYPASSRLS PASSWORD '{password}'"
                ),
                role = role,
                password = password
            );
            conn.execute(&sql).await?;
            passwords.push(password);
        }
        let [apply, fixture, assertion] = &roles;
        conn.execute(&format!("CREATE DATABASE pbps_test OWNER {apply}"))
            .await?;
        let permissions = format!(
            concat!(
                "REVOKE ALL ON DATABASE pbps_test FROM PUBLIC; ",
                "GRANT CONNECT ON DATABASE pbps_test TO {apply}, {fixture}, {assertion}; ",
                "GRANT TEMP ON DATABASE pbps_test TO {fixture}"
            ),
            apply = apply,
            fixture = fixture,
            assertion = assertion
        );
        conn.execute(&permissions).await?;
        drop(conn);
        let db = format!("{admin} dbname=pbps_test");
        let mut conn = Conn::connect(Driver::Postgres, &db).await?;
        conn.execute("REVOKE CREATE ON SCHEMA public FROM PUBLIC")
            .await?;
        drop(conn);
        let connection =
            |role: &str, password: &str| format!("{db} user={role} password={password}");
        let key = scratch.fingerprint_key()?;
        Ok(Self {
            scratch,
            target: Target::test_owned(
                connection(apply, &passwords[0]),
                roles
                    .iter()
                    .map(|r| r.as_str().into())
                    .chain([provision_role])
                    .collect(),
            ),
            fixture: connection(fixture, &passwords[1]),
            assertion: connection(assertion, &passwords[2]),
            roles,
            key,
        })
    }
    async fn admin(&self) -> anyhow::Result<Conn> {
        let db = format!("{} dbname=pbps_test", self.scratch.connection());
        Ok(Conn::connect(Driver::Postgres, &db).await?)
    }
    pub async fn revoke_fixture_routines(&self) -> anyhow::Result<()> {
        let mut conn = self.admin().await?;
        let fixture = &self.roles[1];
        // Temporary runner grants are not declaration grants; leave the ordinary
        // rebuild ACL guard intact and refresh these only before fixture SQL.
        let sql = format!(
            concat!(
                "SELECT DISTINCT n.nspname AS schema FROM pg_catalog.pg_proc p ",
                "JOIN pg_catalog.pg_namespace n ON n.oid=p.pronamespace ",
                "CROSS JOIN LATERAL pg_catalog.aclexplode(p.proacl) a ",
                "WHERE a.grantee='{fixture}'::pg_catalog.regrole ",
                "AND a.privilege_type='EXECUTE'"
            ),
            fixture = fixture
        );
        for row in conn.query(&sql).await? {
            let namespace = quoted(
                row.try_get::<&str>("schema")?
                    .context("missing routine namespace")?,
            )?;
            conn.execute(&format!(
                "REVOKE EXECUTE ON ALL ROUTINES IN SCHEMA {namespace} FROM {fixture}"
            ))
            .await?;
        }
        Ok(())
    }
    // Prepare only the current revision, before its capture/plan. Preparing a
    // future union would make those future roles unmanaged in an earlier hop.
    pub async fn prepare(&self, schema: &Schema) -> anyhow::Result<()> {
        let mut conn = self.admin().await?;
        let apply = &self.roles[0];
        // Ordinary bootstrap requires cluster roles to exist (ADR-0010 §3).
        // Only this owned cluster's administrator supplies those prerequisites;
        // roles keep their ordinary scope and existing image roles stay untouched.
        let roles = schema.named_roles();
        for name in pbps_pg::roles::missing_roles(&mut conn, &roles.iter().collect()).await? {
            let role = quoted(name.raw())?;
            conn.execute(&format!(
                "CREATE ROLE {role} NOLOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION NOBYPASSRLS"
            ))
            .await?;
        }
        // Namespaces are provisioning prerequisites of ordinary bootstrap,
        // not managed changes. Reuse its inventory in this fresh owned database.
        for namespace in crate::deploy::schemas_declared(schema).into_keys() {
            conn.execute(&format!(
                "CREATE SCHEMA IF NOT EXISTS {} AUTHORIZATION {apply}",
                quoted(&namespace)?
            ))
            .await?;
        }
        Ok(())
    }
    pub fn target(&self) -> &Target {
        &self.target
    }
    pub fn assertion(&self) -> &str {
        &self.assertion
    }
    pub fn project(&self, input: &Assembled) -> pbps_config::Project {
        let source = &input.config;
        // A project default may select an unowned server. Construct explicitly
        // so a new runtime setting requires a decision instead of leaking in.
        let config = pbps_config::Config {
            dialect: source.dialect,
            databases: None,
            trusted_roles: source.trusted_roles.clone(),
            pin_type_routines: source.pin_type_routines,
            schema_dir: source.schema_dir.clone(),
            ids_file: source.ids_file.clone(),
            environments: std::collections::BTreeMap::from([(
                "test".into(),
                pbps_config::Environment {
                    url_env: "PBPS_TEST_UNUSED".into(),
                    description: None,
                    resolve_with: None,
                    fingerprint_key_env: None,
                    fingerprint_key_file: Some(self.key.clone()),
                    rehearse: None,
                },
            )]),
            hooks: Default::default(),
            unmanaged: source.unmanaged,
            dev: None,
            resolvers: Default::default(),
            resolve_with: None,
            max_data_rows: source.max_data_rows,
            partition_runway: source.partition_runway.clone(),
            policies: source.policies.clone(),
            consumers: source.consumers.clone(),
        };
        pbps_config::Project {
            root: self
                .key
                .parent()
                .expect("the journal key has a parent")
                .to_owned(),
            config,
        }
    }
    pub async fn grants(&self, schema: &Schema) -> anyhow::Result<()> {
        let mut conn = self.admin().await?;
        let [_, fixture, assertion] = &self.roles;
        // Check every permanent namespace, including those with no managed table.
        // Engine identity, not a pg_temp name-prefix guess, excludes temp helpers.
        let sql = format!(
            concat!(
                "SELECT pg_catalog.has_database_privilege('{fixture}', current_database(), 'CREATE') ",
                "OR pg_catalog.has_database_privilege('{assertion}', current_database(), 'CREATE') ",
                "OR EXISTS (SELECT 1 FROM pg_catalog.pg_namespace n ",
                "WHERE n.oid <> pg_catalog.pg_my_temp_schema() ",
                "AND NOT pg_catalog.pg_is_other_temp_schema(n.oid) AND (",
                "pg_catalog.has_schema_privilege('{fixture}', n.oid, 'CREATE') OR ",
                "pg_catalog.has_schema_privilege('{assertion}', n.oid, 'CREATE'))) AS unsafe"
            ),
            fixture = fixture,
            assertion = assertion
        );
        let rows = conn.query(&sql).await?;
        let permanent_create = rows
            .first()
            .context("missing CREATE privilege result")?
            .try_get::<bool>("unsafe")?;
        ensure!(
            permanent_create == Some(false),
            "fixture/assertion privileges permit permanent CREATE; remove broad grants, including any PUBLIC grant"
        );
        for id in schema.modules.keys() {
            let pbps_model::ModuleId::Routine(routine) = id else {
                continue;
            };
            let namespace = quoted(&routine.name.schema)?;
            let signature = format!(
                "{}.{}({})",
                namespace,
                quoted(&routine.name.name)?,
                routine
                    .args
                    .iter()
                    .map(|a| a.raw())
                    .collect::<Vec<_>>()
                    .join(",")
            );
            // Declared routines are trusted schema code (ADR-0019 §3/§6).
            // Ordinary writers must execute invoker callees even when PostgreSQL
            // records no dependency for a string/PLpgSQL body or a domain default.
            // This changes neither PUBLIC nor the assertion identity's grants.
            conn.execute(&format!(
                concat!(
                    "GRANT USAGE ON SCHEMA {namespace} TO {fixture}; ",
                    "GRANT EXECUTE ON ROUTINE {signature} TO {fixture}"
                ),
                namespace = namespace,
                signature = signature,
                fixture = fixture
            ))
            .await?;
        }
        for (name, declared) in &schema.tables {
            let t = table(name)?;
            let namespace = quoted(&name.schema)?;
            let sql = format!(
                concat!(
                    "GRANT USAGE ON SCHEMA {namespace} TO {fixture}, {assertion}; ",
                    "REVOKE ALL ON {t} FROM {fixture}, {assertion}; ",
                    "GRANT SELECT ON {t} TO {fixture}, {assertion}"
                ),
                namespace = namespace,
                t = t,
                fixture = fixture,
                assertion = assertion
            );
            conn.execute(&sql).await?;
            if declared.data.is_none() {
                conn.execute(&format!("GRANT INSERT, UPDATE, DELETE ON {t} TO {fixture}"))
                    .await?;
                // Ownership and default dependencies name only this table's allocators.
                let literal = t.replace('\'', "''");
                let sql = format!(
                    concat!(
                        "SELECT DISTINCT n.nspname AS schema, s.relname AS name ",
                        "FROM pg_catalog.pg_class s JOIN pg_catalog.pg_namespace n ON n.oid=s.relnamespace ",
                        "WHERE s.relkind='S' AND (EXISTS (SELECT 1 FROM pg_catalog.pg_depend d ",
                        "WHERE d.classid='pg_class'::regclass AND d.objid=s.oid ",
                        "AND d.refobjid='{relation}'::regclass AND d.deptype IN ('a','i')) ",
                        "OR EXISTS (SELECT 1 FROM pg_catalog.pg_depend d JOIN pg_catalog.pg_attrdef a ",
                        "ON d.classid='pg_attrdef'::regclass AND d.objid=a.oid ",
                        "WHERE a.adrelid='{relation}'::regclass AND d.refclassid='pg_class'::regclass ",
                        "AND d.refobjid=s.oid))"
                    ),
                    relation = literal
                );
                let rows = conn.query(&sql).await?;
                for r in rows {
                    let sequence = TableName::new(
                        r.try_get::<&str>("schema")?
                            .context("allocator schema missing")?,
                        r.try_get::<&str>("name")?
                            .context("allocator name missing")?,
                    );
                    conn.execute(&format!(
                        "GRANT USAGE ON SEQUENCE {} TO {fixture}",
                        table(&sequence)?
                    ))
                    .await?;
                }
            }
            let literal = t.replace('\'', "''");
            // Both identities use one effective test; data: controls fixture DML.
            let sql = format!(
                concat!(
                    "SELECT EXISTS (SELECT 1 FROM (VALUES ('{assertion}',true), ",
                    "('{fixture}',{reference})) actor(principal,deny_write) ",
                    "WHERE actor.deny_write AND (pg_catalog.has_table_privilege(",
                    "actor.principal::pg_catalog.name,'{relation}', ",
                    "'INSERT,UPDATE,DELETE,TRUNCATE,REFERENCES,TRIGGER') OR ",
                    "pg_catalog.has_any_column_privilege(",
                    "actor.principal::pg_catalog.name,'{relation}', ",
                    "'INSERT,UPDATE,REFERENCES'))) AS unsafe"
                ),
                relation = literal,
                reference = declared.data.is_some(),
                fixture = fixture,
                assertion = assertion,
            );
            let rows = conn.query(&sql).await?;
            ensure!(
                rows.first()
                    .context("missing privilege result")?
                    .try_get::<bool>("unsafe")?
                    == Some(false),
                "the effective SQL privileges for {} exceed the fixture/assertion boundary; remove broad grants, including any PUBLIC grant",
                name.shown()
            );
        }
        Ok(())
    }
    pub async fn fixtures(&self) -> anyhow::Result<Conn> {
        Ok(Conn::connect(Driver::Postgres, &self.fixture).await?)
    }
}
