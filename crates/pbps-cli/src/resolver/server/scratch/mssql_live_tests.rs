//! SQL Server's acquisition boundary, exercised through the shared producer.

use super::super::{BindingRequest, Error, ProduceError, Unsealed};
use super::fixtures::{ENVIRONMENT, KEY_LINE, ProjectKey};
use super::{Provenance, Standard};
use pbps_db::{Conn, Driver};
use pbps_model::{Hints, IdsFile, Schema};
use pbps_mssql::resolver::scratch as sql;

mod catalog_cases {
    include!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../pbps-mssql/tests/support/scratch_catalogs.rs"
    ));
}

pub(super) fn provenance() -> Provenance {
    Provenance {
        same_instance: pbps_model::resolver::SameInstance::Refused,
        ..Provenance::server()
    }
}

pub(super) fn server(variable: &str) -> String {
    std::env::var(variable)
        .unwrap_or_else(|_| panic!("{variable} must name the owned SQL Server resolver fixture"))
}

#[tokio::test]
#[ignore = "needs the owned SQL Server resolver fixtures"]
async fn mssql_dbo_compile_session_keeps_the_marker_and_cleans_through_a_fresh_sandbox() {
    use pbps_mssql::resolver::{authorization as auth, environment};
    let primary = server("PBPS_TEST_DB");
    let source = server("PBPS_TEST_MSSQL_SCRATCH_DB");
    let (name, on) = create_database(&source).await;
    let mut owner = connection(&on).await;
    let marker = sql::mark(&mut owner, &format!("{:032x}", rand::random::<u128>()))
        .await
        .unwrap();
    let databases = sql::database_names(&mut owner).await.unwrap();
    let mut target = connection(&primary).await;
    let schemas = vec!["dbo".to_owned()];
    let context = auth::read(&mut target, &schemas).await.unwrap();
    let facts = environment::read(&mut target, &environment::Scope { schemas: &schemas })
        .await
        .unwrap();
    let map = auth::PrincipalMap::generate(&context, &[], "cli2267");
    let sandbox = "pbps_cli_sandbox2267";
    sql::create_sandbox(&mut owner, sandbox).await.unwrap();
    let mut compile = connection(&on).await;
    assert!(sql::marked(&mut compile, &marker).await.unwrap());
    sql::enter_sandbox(&mut compile, sandbox).await.unwrap();
    sql::sandbox_checks(&mut compile, &databases, &name)
        .await
        .unwrap();
    auth::reconstruct_database(&mut compile, &map, &context)
        .await
        .unwrap();
    let entered = auth::enter_sandbox_deployer(
        &mut compile,
        &map,
        &context,
        &facts,
        &auth::SandboxScope {
            sandbox,
            scratch: &name,
            databases: &databases,
        },
    )
    .await
    .unwrap();
    assert!(entered.is_dbo());
    assert!(sql::marked(&mut compile, &marker).await.unwrap());
    // dbo carries extra DENY ONLY entries (dbo, db_owner) into master,
    // tempdb and msdb only when the scratch's owner also owns them, as sa does
    // on the Linux fixtures. A LocalDB scratch's Windows owner does not, and
    // its token there is guest and public alone (measured on 17.0). Where the
    // extras exist, the ordinary check must still reject them.
    let owns_system = owner
        .query(&format!(
            "SELECT COUNT(*) AS n FROM sys.databases s JOIN sys.databases d \
             ON d.owner_sid = s.owner_sid WHERE s.name = N'{name}' \
             AND d.name IN (N'master', N'tempdb', N'msdb');"
        ))
        .await
        .unwrap()[0]
        .try_get::<i32>("n")
        .unwrap()
        != Some(0);
    assert_eq!(
        sql::sandbox_checks(&mut compile, &databases, &name)
            .await
            .is_err(),
        owns_system
    );
    compile
        .execute("CREATE TABLE dbo.compiled2267(i int);")
        .await
        .unwrap();
    drop(compile);
    let mut cleanup = connection(&on).await;
    assert!(sql::marked(&mut cleanup, &marker).await.unwrap());
    sql::enter_sandbox(&mut cleanup, sandbox).await.unwrap();
    sql::sandbox_checks(&mut cleanup, &databases, &name)
        .await
        .unwrap();
    sql::inventory::cleanup(&mut cleanup, Some(sandbox))
        .await
        .unwrap();
    assert!(
        sql::inventory::read(&mut cleanup, Some(sandbox))
            .await
            .unwrap()
            .is_empty()
    );
    drop(cleanup);
    owner
        .execute(&format!("DROP USER [{sandbox}];"))
        .await
        .unwrap();
    assert!(
        sql::inventory::read(&mut owner, None)
            .await
            .unwrap()
            .is_empty()
    );
    drop(owner);
    drop_database(&source, &name).await;
}

pub(super) async fn connection(url: &str) -> Conn {
    Conn::connect(Driver::Mssql, url).await.unwrap()
}

// Repeated acquisitions inside nested Docker fixtures otherwise copy their
// futures into a debug poll frame that exhausts the default test-thread stack.
pub(super) fn refusal<'a>(
    target: &'a str,
    scratch: &'a str,
) -> impl Future<Output = ProduceError> + 'a {
    Box::pin(refusal_inner(target, scratch))
}

async fn refusal_inner(target: &str, scratch: &str) -> ProduceError {
    refusal_file_inner(target, scratch, None).await
}

async fn refusal_file_inner(target: &str, scratch: &str, externals: Option<&str>) -> ProduceError {
    refusal_policy_inner(target, scratch, externals, &provenance()).await
}

pub(super) async fn refusal_policy_inner(
    target: &str,
    scratch: &str,
    externals: Option<&str>,
    provenance: &Provenance,
) -> ProduceError {
    declared_refusal(target, scratch, &Declared::default(), externals, provenance).await
}

/// What a SQL Server run ends on. A run that completes has already released
/// everything it created, so its result carries no scratch state.
pub(super) type Outcome = Result<Unsealed<pbps_mssql::Mssql>, ProduceError>;

/// What `why` says of a run that completed.
pub(super) const COMPLETED: &str = "the run completed";

/// The failure text of a run, or `COMPLETED`.
pub(super) fn why(outcome: &Outcome) -> String {
    outcome
        .as_ref()
        .err()
        .map_or_else(|| COMPLETED.into(), ToString::to_string)
}

pub(super) fn outcome<'a>(target: &'a str, scratch: &'a str) -> impl Future<Output = Outcome> + 'a {
    Box::pin(outcome_file_inner(target, scratch, None))
}

pub(super) async fn outcome_file_inner(
    target: &str,
    scratch: &str,
    externals: Option<&str>,
) -> Outcome {
    outcome_policy_inner(target, scratch, externals, &provenance()).await
}

pub(super) async fn outcome_policy_inner(
    target: &str,
    scratch: &str,
    externals: Option<&str>,
    provenance: &Provenance,
) -> Outcome {
    declared_outcome(target, scratch, &Declared::default(), externals, provenance).await
}

/// The managed declarations a run is asked about: what the target holds as
/// managed, and what the plan makes of it.
#[derive(Default)]
struct Declared {
    base: Schema,
    desired: Schema,
}

/// A run expected to refuse. One that completes is a test failure here.
async fn declared_refusal(
    target: &str,
    scratch: &str,
    declared: &Declared,
    externals: Option<&str>,
    provenance: &Provenance,
) -> ProduceError {
    match declared_outcome(target, scratch, declared, externals, provenance).await {
        Err(error) => error,
        Ok(_) => panic!("the run was expected to refuse, but it completed"),
    }
}

/// What a declared run needs besides the connections, formed as the planner
/// forms it.
struct Begun {
    key: ProjectKey,
    /// The cluster identity the target string reached, as the planning read records it.
    identity: String,
    base_ids: IdsFile,
    desired_ids: IdsFile,
    bootstrap: pbps_diff::order::DiffPlan,
    dependencies: pbps_model::dependency::Dependencies,
}

async fn begin(target: &str, declared: &Declared) -> Begun {
    let mut target_session = connection(target).await;
    let identity = sql::cluster_identity(&mut target_session).await.unwrap();
    let mut key = ProjectKey::new(true);
    let config = format!(
        "dialect: mssql\nenvironments:\n  fixture:\n    url_env: PBPS_2236_UNUSED\n{KEY_LINE}"
    );
    let path = key.project.root.join("pbps.yml");
    std::fs::write(&path, &config).unwrap();
    key.project.config = pbps_config::Config::parse(&config, &path).unwrap();
    // The desired side continues the base side's identities, as the recorded
    // ids file does; resolved apart, a kept role would be a different one.
    let ids = |schema: &Schema, previous: &IdsFile, intents: &[pbps_model::Intent]| {
        pbps_diff::resolve(
            schema,
            previous,
            intents,
            &pbps_diff::Context {
                operator: "resolver-live-tests".into(),
                today: "2026-10-25".into(),
            },
        )
        .unwrap()
        .ids
    };
    // A table the declarations no longer hold is dropped on purpose, as its
    // recorded intent says.
    let drops: Vec<_> = declared
        .base
        .tables
        .keys()
        .filter(|table| !declared.desired.tables.contains_key(*table))
        .map(|table| pbps_model::Intent::DropTable {
            table: table.clone(),
            reason: pbps_model::Reason::new("dropped by the live test").unwrap(),
        })
        .collect();
    let base_ids = ids(&declared.base, &IdsFile::default(), &[]);
    let desired_ids = ids(&declared.desired, &base_ids, &drops);
    let nothing = (Schema::default(), IdsFile::default());
    // As the planner forms it: the differ's plan from nothing to the
    // declarations, which may name a function a computed column calls
    // before it exists (`ordinary` refuses that, and is not the bootstrap).
    let bootstrap = pbps_diff::diff(
        pbps_diff::Side {
            schema: &nothing.0,
            ids: &nothing.1,
        },
        pbps_diff::Side {
            schema: &declared.desired,
            ids: &desired_ids,
        },
        &pbps_mssql::Mssql,
        &Hints::default(),
    )
    .expect("the declared schema has a bootstrap plan");
    Begun {
        key,
        identity,
        base_ids,
        desired_ids,
        bootstrap,
        dependencies: Default::default(),
    }
}

impl Begun {
    fn sides<'a>(&'a self, declared: &'a Declared) -> (pbps_diff::Side<'a>, pbps_diff::Side<'a>) {
        (
            pbps_diff::Side {
                schema: &declared.base,
                ids: &self.base_ids,
            },
            pbps_diff::Side {
                schema: &declared.desired,
                ids: &self.desired_ids,
            },
        )
    }

    fn binding<'a>(&'a self, declared: &'a Declared) -> BindingRequest<'a> {
        BindingRequest {
            dependencies: &self.dependencies,
            bootstrap: &self.bootstrap,
            base: &declared.base,
            desired: &declared.desired,
        }
    }
}

/// The run's own result: a plan from evidence, or the refusal that stopped it.
async fn declared_outcome(
    target: &str,
    scratch: &str,
    declared: &Declared,
    externals: Option<&str>,
    provenance: &Provenance,
) -> Outcome {
    let run = begin(target, declared).await;
    let (base, desired) = run.sides(declared);
    super::produce_unsealed::<pbps_mssql::Mssql>(
        Driver::Mssql,
        scratch,
        target,
        &run.identity,
        &run.binding(declared),
        base,
        desired,
        &Hints::default(),
        &[],
        &run.key.project,
        Some(ENVIRONMENT),
        Standard::Mssql,
        externals,
        provenance,
    )
    .await
}

/// The sealed plan of a declared run, past the publication recheck, or the
/// refusal that stopped it. `planned_identity` overrides the cluster identity
/// the planning read recorded, to give the recheck another cluster to find.
async fn declared_plan(
    target: &str,
    scratch: &str,
    declared: &Declared,
    planned_identity: Option<&str>,
) -> Result<
    (
        crate::resolver::server::ResolvedPlan<pbps_diff::order::Canonical<pbps_mssql::Mssql>>,
        Begun,
    ),
    ProduceError,
> {
    let run = begin(target, declared).await;
    let (base, desired) = run.sides(declared);
    let plan = super::produce::<pbps_mssql::Mssql>(
        Driver::Mssql,
        scratch,
        target,
        planned_identity.unwrap_or(&run.identity),
        &run.binding(declared),
        base,
        desired,
        &Hints::default(),
        &[],
        &run.key.project,
        Some(ENVIRONMENT),
        Standard::Mssql,
        None,
        &provenance(),
    )
    .await?;
    Ok((plan, run))
}

pub(super) async fn create_database(url: &str) -> (String, String) {
    let name = format!("pbps2236_{:032x}", rand::random::<u128>());
    connection(url)
        .await
        .execute(&format!("CREATE DATABASE [{name}];"))
        .await
        .unwrap();
    let on = format!("{url};Database={name}");
    assert_eq!(
        sql::database(&mut connection(&on).await)
            .await
            .unwrap()
            .name,
        name
    );
    (name, on)
}

pub(super) async fn drop_database(url: &str, name: &str) {
    connection(url)
        .await
        .execute(&format!(
            "USE master; ALTER DATABASE [{name}] SET SINGLE_USER WITH ROLLBACK IMMEDIATE; \
             DROP DATABASE [{name}];"
        ))
        .await
        .unwrap();
}

async fn supplied_contract(target: &str, server: &str) {
    let (name, on) = create_database(server).await;
    let login = format!("pbps2236_owner_{:032x}", rand::random::<u128>());
    let password = sql::password(&format!("{:032x}", rand::random::<u128>()));
    let mut admin = connection(server).await;
    admin
        .execute(&format!(
            "CREATE LOGIN [{login}] WITH PASSWORD=N'{password}',CHECK_POLICY=ON; ALTER \
             AUTHORIZATION ON DATABASE::[{name}] TO [{login}];"
        ))
        .await
        .unwrap();
    let supplied = format!("{server};Database={name};User Id={login};Password={password}");
    let positive = outcome(target, &supplied).await;
    let clean = sql::inventory::read(&mut connection(&on).await, None)
        .await
        .unwrap();
    let (guest_name, guest) = create_database(server).await;
    connection(&guest)
        .await
        .execute("GRANT CONNECT TO guest;")
        .await
        .unwrap();
    let guest_refusal = refusal(target, &supplied).await.to_string();
    connection(&guest)
        .await
        .execute("REVOKE CONNECT FROM guest;")
        .await
        .unwrap();
    drop_database(server, &guest_name).await;
    admin
        .execute(&format!("DENY VIEW ANY DATABASE TO [{login}];"))
        .await
        .unwrap();
    let hidden = refusal(target, &supplied).await.to_string();
    admin
        .execute(&format!("REVOKE VIEW ANY DATABASE FROM [{login}];"))
        .await
        .unwrap();
    let mut dirty = connection(&on).await;
    let backup = format!("/tmp/{name}.bak");
    dirty
        .execute(&format!(
            "ALTER DATABASE [{name}] SET RECOVERY FULL; BACKUP DATABASE [{name}] TO \
             DISK=N'{backup}' WITH INIT;"
        ))
        .await
        .unwrap();
    dirty
        .execute(
            "CREATE TABLE dbo.log_work(i int,payload char(8000)); INSERT dbo.log_work SELECT \
             TOP(2000) ROW_NUMBER() OVER(ORDER BY(SELECT NULL)),'x' FROM sys.all_objects a \
             CROSS JOIN sys.all_objects b; DROP TABLE dbo.log_work; CHECKPOINT;",
        )
        .await
        .unwrap();
    let status = dirty
        .query("SELECT log_reuse_wait_desc FROM sys.databases WHERE database_id=DB_ID();")
        .await
        .unwrap();
    let full_recovery = outcome(target, &supplied).await;
    dirty
        .execute(&format!(
            "EXEC master.sys.xp_delete_files N'{backup}'; ALTER DATABASE [{name}] SET \
             AUTO_SHRINK ON;"
        ))
        .await
        .unwrap();
    let persistent = refusal(target, &supplied).await.to_string();
    dirty
        .execute(&format!("ALTER DATABASE [{name}] SET AUTO_SHRINK OFF;"))
        .await
        .unwrap();
    dirty
        .execute(
            "CREATE COLUMN MASTER KEY cmk WITH(KEY_STORE_PROVIDER_NAME=N'MSSQL_CERTIFICATE_STORE',\
             KEY_PATH=N'CurrentUser/My/pbps2236'); CREATE COLUMN ENCRYPTION KEY cek WITH \
             VALUES(COLUMN_MASTER_KEY=cmk,ALGORITHM=N'RSA_OAEP',ENCRYPTED_VALUE=0x01020304); EXEC \
             sys.sp_addextendedproperty @name=N'run_property',@value=N'dirty';",
        )
        .await
        .unwrap();
    let dirty_refusal = refusal(target, &supplied).await.to_string();
    let untouched = sql::inventory::read(&mut dirty, None)
        .await
        .unwrap()
        .names();
    sql::inventory::cleanup(&mut dirty, None).await.unwrap();
    let rechecked = sql::inventory::read(&mut dirty, None).await.unwrap();
    drop(dirty);
    let mut system_refusals = Vec::new();
    let guest_before = warm2585::guest_context(server).await;
    assert!(
        !guest_before
            .iter()
            .any(|entry| entry.starts_with("permission:0:0:0:CONTROL:")
                || entry == "role:SQLAgentOperatorRole"
                || entry == "schema:pbps2236_guest_owned"),
        "the warm msdb guest context is already occupied: {guest_before:?}"
    );
    for (change, restore) in [
        (
            "USE msdb; GRANT CONTROL TO guest;",
            "USE msdb; REVOKE CONTROL FROM guest;",
        ),
        (
            "USE msdb; ALTER ROLE SQLAgentOperatorRole ADD MEMBER guest;",
            "USE msdb; ALTER ROLE SQLAgentOperatorRole DROP MEMBER guest;",
        ),
        (
            "USE msdb; EXEC(N'CREATE SCHEMA pbps2236_guest_owned AUTHORIZATION guest');",
            "USE msdb; DROP SCHEMA pbps2236_guest_owned;",
        ),
    ] {
        let (target, supplied) = (target.to_owned(), supplied.clone());
        let result = warm2585::restoring(server, change, restore, async move {
            refusal(&target, &supplied).await.to_string()
        })
        .await;
        assert_eq!(warm2585::guest_context(server).await, guest_before);
        system_refusals.push(result.unwrap());
    }
    drop_database(server, &name).await;
    admin
        .execute(&format!("USE master; DROP LOGIN [{login}];"))
        .await
        .unwrap();
    assert!(positive.is_ok(), "{}", why(&positive));
    assert!(clean.is_empty(), "{:?}", clean.names());
    assert!(guest_refusal.contains(&guest_name), "{guest_refusal}");
    assert!(hidden.contains("VIEW ANY DATABASE"), "{hidden}");
    assert_eq!(
        status[0].try_get::<&str>("log_reuse_wait_desc").unwrap(),
        Some("LOG_BACKUP")
    );
    assert!(full_recovery.is_ok(), "{}", why(&full_recovery));
    assert!(persistent.contains("is_auto_shrink_on"), "{persistent}");
    assert!(
        dirty_refusal.contains("COLUMN MASTER KEY")
            && dirty_refusal.contains("COLUMN ENCRYPTION KEY")
            && dirty_refusal.contains("extended property"),
        "{dirty_refusal}"
    );
    for kind in [
        "COLUMN MASTER KEY",
        "COLUMN ENCRYPTION KEY",
        "extended property",
    ] {
        assert!(
            untouched.iter().any(|name| name.contains(kind)),
            "{untouched:?}"
        );
    }
    assert!(rechecked.is_empty(), "{:?}", rechecked.names());
    for refusal in system_refusals {
        assert!(refusal.contains("msdb"), "{refusal}");
    }
}

async fn inherited_model_refusal(target: &str, server: &str) {
    let name = format!("pbps2236_inherited_{:032x}", rand::random::<u128>());
    let mut admin = connection(server).await;
    let target = target.to_owned();
    let source = server.to_owned();
    let result = warm2585::restoring(
        server,
        &format!("EXEC(N'USE model; CREATE TABLE dbo.{name}(i int);');"),
        &format!("EXEC(N'USE model; DROP TABLE dbo.{name};');"),
        async move { refusal(&target, &source).await },
    )
    .await;
    let row = admin
        .query(&format!(
            "SELECT OBJECT_ID(N'model.dbo.{name}') AS remaining;"
        ))
        .await
        .unwrap();
    assert_eq!(row[0].try_get::<i32>("remaining").unwrap(), None);
    let refusal = result.unwrap().to_string();
    assert!(refusal.contains(&name), "{refusal}");
}

#[tokio::test]
#[ignore = "needs the owned SQL Server resolver fixtures"]
async fn mssql_supplied_owner_needs_no_creation_and_changed_visibility_or_system_context_refuses() {
    let primary = server("PBPS_TEST_DB");
    supplied_contract(&primary, &primary).await;
}

// Keep the historical identity in receipts; its full SQL contract is now warm.
#[tokio::test]
#[ignore = "needs the owned SQL Server resolver fixtures"]
async fn mssql_docker_supplied_inventory_and_sandbox_use_the_same_contract() {
    fn case<'a>(
        target: &'a str,
        scratch: &'a str,
    ) -> std::pin::Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
        Box::pin(async move {
            let same = refusal(scratch, scratch).await;
            assert!(
                matches!(same, ProduceError::Run(Error::Unusable(ref reason))
                if reason.contains("target database")),
                "{same}"
            );
            let owned = outcome(target, scratch).await;
            assert!(owned.is_ok(), "{}", why(&owned));
            supplied_contract(target, scratch).await;
            inherited_model_refusal(target, scratch).await;
        })
    }
    reconstruct2374::both(case).await;
}

/// Docker acquisition still gets a real witness; SQL semantics need no repeat.
#[tokio::test]
#[ignore = "needs Docker and the owned SQL Server resolver fixtures"]
async fn mssql_docker_acquisition_keeps_the_run_owned_contract() {
    let target = server("PBPS_TEST_DB");
    let image = server("PBPS_TEST_RESOLVER_MSSQL_IMAGE");
    let identity = sql::cluster_identity(&mut connection(&target).await)
        .await
        .unwrap();
    let hostname = std::sync::Arc::new(tokio::sync::Mutex::new(String::new()));
    let recorded = hostname.clone();
    let target_for_run = target.clone();
    super::container::with_container(
        Driver::Mssql,
        &target,
        &identity,
        &image,
        pbps_config::resolver::PullPolicy::Never,
        async move |url: &str| {
            let rows = connection(url)
                .await
                .query("SELECT CONVERT(nvarchar(128),SERVERPROPERTY('ServerName')) AS name;")
                .await
                .unwrap();
            *recorded.lock().await = rows[0].try_get::<&str>("name").unwrap().unwrap().to_owned();
            let scratch = url.to_owned();
            warm2585::run(&[url], async move {
                let same = refusal(&scratch, &scratch).await;
                assert!(
                    matches!(same, ProduceError::Run(Error::Unusable(ref reason))
                    if reason.contains("target database")),
                    "{same}"
                );
                let owned = outcome(&target_for_run, &scratch).await;
                assert!(owned.is_ok(), "{}", why(&owned));
                supplied_contract(&target_for_run, &scratch).await;
                inherited_model_refusal(&target_for_run, &scratch).await;
            })
            .await
            .unwrap();
            Ok(())
        },
    )
    .await
    .unwrap();
    // A failed inspect is not proof of absence: enumerate successfully, then
    // reject the exact server identity observed inside this container.
    let hostname = hostname.lock().await.clone();
    assert!(
        hostname.len() >= 12 && hostname.bytes().all(|b| b.is_ascii_hexdigit()),
        "{hostname}"
    );
    let listed = tokio::process::Command::new("docker")
        .args(["ps", "-a", "--no-trunc", "--format", "{{.ID}}"])
        .output()
        .await
        .unwrap();
    assert!(listed.status.success(), "{:?}", listed.status);
    for id in String::from_utf8(listed.stdout).unwrap().lines() {
        assert!(
            id.len() == 64 && id.bytes().all(|b| b.is_ascii_hexdigit()),
            "{id}"
        );
        assert!(
            !id.starts_with(hostname.as_str()),
            "owned container survived release: {id}"
        );
    }
}

#[tokio::test]
#[ignore = "needs the owned SQL Server resolver fixtures"]
async fn mssql_run_owned_acquisition_needs_only_login_and_database_creation_privileges() {
    let target = server("PBPS_TEST_DB");
    let scratch = server("PBPS_TEST_MSSQL_SCRATCH_DB");
    let login = format!("pbps2236_provisioner_{:032x}", rand::random::<u128>());
    let password = sql::password(&format!("{:032x}", rand::random::<u128>()));
    let mut admin = connection(&scratch).await;
    admin
        .execute(&format!(
            "CREATE LOGIN [{login}] WITH PASSWORD=N'{password}',CHECK_POLICY=ON; GRANT ALTER \
             ANY LOGIN TO [{login}]; GRANT CREATE ANY DATABASE TO [{login}];"
        ))
        .await
        .unwrap();
    let source = format!("{scratch};Database=master;User Id={login};Password={password}");
    let outcome = outcome(&target, &source).await;
    let remaining = admin
        .query(&format!(
            "SELECT name FROM sys.databases WHERE owner_sid=SUSER_SID(N'{login}');"
        ))
        .await
        .unwrap();
    admin
        .execute(&format!("DROP LOGIN [{login}];"))
        .await
        .unwrap();
    assert!(outcome.is_ok(), "{}", why(&outcome));
    assert!(remaining.is_empty());
}

#[tokio::test]
#[ignore = "needs the owned SQL Server resolver fixtures"]
async fn mssql_supplied_cleanup_names_the_retained_sandbox_after_failure() {
    let primary = server("PBPS_TEST_DB");
    let schema = Schema::default();
    let ids = IdsFile::default();
    let side = pbps_diff::Side {
        schema: &schema,
        ids: &ids,
    };
    let bootstrap =
        pbps_diff::resolver::ordinary(side, side, &pbps_mssql::Mssql, &Hints::default())
            .expect("an empty SQL Server schema has an empty ordinary plan");
    let request =
        super::super::producer::request(Driver::Mssql, side, side, &Hints::default(), &[]).unwrap();
    for missing_marker in [false, true] {
        let (database, on) = create_database(&primary).await;
        let outcome: Result<_, pbps_db::DbError> = async {
            let mut target = Conn::connect(Driver::Mssql, &primary).await?;
            let mut admin = Conn::connect(Driver::Mssql, &on).await?;
            let identity = sql::cluster_identity(&mut target).await?;
            let tokens = super::Tokens::generate();
            let provenance = provenance();
            let (provisioning, mut state) = super::engine::scratch(
                Driver::Mssql,
                &mut target,
                &mut admin,
                &tokens,
                &identity,
                &on,
                &request,
                &Standard::Mssql,
                None,
                &provenance,
            )
            .await
            .map_err(|error| pbps_db::DbError::Refused(error.to_string()))?;
            let (catalog, authorization) =
                crate::resolver::scope::read_target(&mut target, Driver::Mssql, &[], &[], &[], &[])
                    .await?;
            let prepared = state
                .prepare(
                    provisioning,
                    &mut admin,
                    &mut target,
                    &catalog,
                    &authorization,
                    &[],
                    &BindingRequest {
                        dependencies: &Default::default(),
                        bootstrap: &bootstrap,
                        base: &schema,
                        desired: &schema,
                    },
                    &[],
                    Some(&crate::resolver::server::scratch::mssql_live_tests::capture_key()),
                )
                .await
                .err();
            let rows = admin
                .query(
                    "SELECT name FROM sys.database_principals \
                     WHERE name LIKE N'pbps[_]sandbox[_]%';",
                )
                .await?;
            let sandbox = rows
                .first()
                .ok_or_else(|| pbps_db::DbError::BadRow("no sandbox was created".into()))?
                .try_get::<&str>("name")?
                .ok_or_else(|| pbps_db::DbError::BadRow("sandbox name is NULL".into()))?
                .to_owned();
            if missing_marker {
                admin
                    .execute(&format!("DROP TABLE [##pbps_{}];", tokens.scratch))
                    .await?;
            } else {
                // An index property has no cleanup rule. The privileged final
                // drop must be skipped, and its retained user must be named.
                admin
                    .execute(
                        "CREATE TABLE dbo.cleanup_refusal(i int); \
                         CREATE INDEX ix ON dbo.cleanup_refusal(i); \
                         EXEC sys.sp_addextendedproperty @name=N'cleanup_property',@value=N'held',\
                         @level0type=N'SCHEMA',@level0name=N'dbo',@level1type=N'TABLE',\
                         @level1name=N'cleanup_refusal',@level2type=N'INDEX',@level2name=N'ix';",
                    )
                    .await?;
            }
            let cleanup = state.release(&mut admin).await;
            let retained = admin
                .query(&format!(
                    "SELECT IS_ROLEMEMBER(N'db_owner',name) AS membership \
                     FROM sys.database_principals WHERE name=N'{sandbox}';"
                ))
                .await?;
            Ok((prepared, cleanup, sandbox, retained))
        }
        .await;
        // Restore before assertions, including when the regression is tested
        // with the reporting fix reverted.
        drop_database(&primary, &database).await;
        let (prepared, cleanup, sandbox, retained) = outcome.unwrap();
        assert!(
            prepared.is_none(),
            "preparation must now succeed: {prepared:?}"
        );
        assert_eq!(retained.len(), 1);
        assert_eq!(retained[0].try_get::<i32>("membership").unwrap(), Some(1));
        let failed = cleanup.unwrap_err();
        let expected = format!("sandbox user {sandbox} (db_owner) in database {database}");
        assert_eq!(
            failed.iter().filter(|name| **name == expected).count(),
            1,
            "{failed:?}"
        );
        let original = if missing_marker {
            "no longer reaches the marked database"
        } else {
            "extended property class 7 cleanup_property"
        };
        assert!(
            failed.iter().any(|name| name.contains(original)),
            "{failed:?}"
        );
    }
}

#[tokio::test]
#[ignore = "needs the owned SQL Server resolver fixtures"]
async fn mssql_supplied_acquisition_refuses_target_and_cleans_after_the_externals_refusal() {
    let primary = server("PBPS_TEST_DB");
    let (name, on) = create_database(&primary).await;
    let target = refusal(&on, &on).await;
    let boundary = outcome(&primary, &on).await;
    let empty = sql::inventory::read(&mut connection(&on).await, None)
        .await
        .unwrap();
    drop_database(&primary, &name).await;
    assert!(
        matches!(
            target,
            ProduceError::Run(Error::Unusable(ref reason)) if reason.contains("target database")
        ),
        "{target}"
    );
    assert!(boundary.is_ok(), "{}", why(&boundary));
    assert!(empty.is_empty(), "{:?}", empty.names());
}

#[tokio::test]
#[ignore = "needs the owned SQL Server resolver fixtures"]
async fn instance_wide_ownership_chaining_refuses_the_scratch_sandbox_and_cleans_setup() {
    let primary = server("PBPS_TEST_DB");
    let scratch = server("PBPS_TEST_MSSQL_SCRATCH_DB");
    let (name, supplied) = create_database(&scratch).await;
    let mut admin = connection(&scratch).await;
    let option =
        |value| format!("EXEC sp_configure 'cross db ownership chaining', {value}; RECONFIGURE;");
    // This group runs serially on its dedicated instances. Joining a task
    // catches a panic in acquisition too, so restoration precedes assertions.
    let measured = tokio::spawn(async move {
        let inventory = async || {
            connection(&scratch)
                .await
                .query(
                    "SELECT N'database:' + name AS name FROM sys.databases UNION ALL \
                     SELECT N'login:' + name FROM sys.server_principals ORDER BY name;",
                )
                .await
                .unwrap()
                .iter()
                .map(|row| row.try_get::<&str>("name").unwrap().unwrap().to_owned())
                .collect::<Vec<_>>()
        };
        let before = inventory().await;
        connection(&scratch)
            .await
            .execute(&option(1))
            .await
            .unwrap();
        // A separate target chooses run-owned provisioning; a target on the
        // scratch instance forces the supplied database path, even as sa.
        let mut refused = Vec::new();
        for (target, source) in [(&primary, &scratch), (&scratch, &supplied)] {
            refused.push(refusal(target, source).await);
        }
        let after_refusal = inventory().await;
        let supplied_after_refusal = sql::inventory::read(&mut connection(&supplied).await, None)
            .await
            .unwrap();
        connection(&scratch)
            .await
            .execute(&option(0))
            .await
            .unwrap();
        let mut allowed = Vec::new();
        for (target, source) in [(&primary, &scratch), (&scratch, &supplied)] {
            allowed.push(outcome(target, source).await);
        }
        let after_control = inventory().await;
        let supplied_after_control = sql::inventory::read(&mut connection(&supplied).await, None)
            .await
            .unwrap();
        (
            before,
            after_refusal,
            after_control,
            supplied_after_refusal,
            supplied_after_control,
            refused,
            allowed,
        )
    })
    .await;
    let restored = admin.execute(&option(0)).await;
    let setting = admin
        .query(
            "SELECT CONVERT(int,value_in_use) AS value_in_use FROM sys.configurations \
             WHERE name=N'cross db ownership chaining';",
        )
        .await;
    drop_database(&server("PBPS_TEST_MSSQL_SCRATCH_DB"), &name).await;
    restored.unwrap();
    assert_eq!(
        setting.unwrap()[0].try_get::<i32>("value_in_use").unwrap(),
        Some(0)
    );
    let (
        before,
        after_refusal,
        after_control,
        supplied_refused,
        supplied_control,
        refused,
        allowed,
    ) = measured.unwrap();
    assert_eq!(
        after_refusal, before,
        "refused setup left a database or login"
    );
    assert_eq!(
        after_control, before,
        "control setup left a database or login"
    );
    for inventory in [supplied_refused, supplied_control] {
        assert!(inventory.is_empty(), "{:?}", inventory.names());
    }
    for outcome in refused {
        assert!(
            matches!(outcome, ProduceError::Run(Error::Read(_))),
            "{outcome}"
        );
        let message = outcome.to_string();
        assert!(message.contains("cross db ownership chaining"), "{message}");
        assert!(
            message.contains("EXEC sp_configure 'cross db ownership chaining', 0; RECONFIGURE;"),
            "{message}"
        );
    }
    for outcome in allowed {
        assert!(outcome.is_ok(), "{}", why(&outcome));
    }
}

#[tokio::test]
#[ignore = "needs the owned SQL Server resolver fixtures"]
async fn mssql_run_owned_acquisition_removes_its_login_database_and_reference_on_refusal() {
    let primary = server("PBPS_TEST_DB");
    let scratch = server("PBPS_TEST_MSSQL_SCRATCH_DB");
    let inventory = async || {
        let rows = connection(&scratch)
            .await
            .query(
                "SELECT name FROM sys.databases WHERE name LIKE N'pbps_scratch_%' UNION ALL \
                 SELECT name FROM sys.server_principals WHERE name LIKE N'pbps_run_%' ORDER \
                 BY name;",
            )
            .await
            .unwrap();
        rows.iter()
            .map(|row| row.try_get::<&str>("name").unwrap().unwrap().to_owned())
            .collect::<Vec<_>>()
    };
    let before = inventory().await;
    let outcome = outcome(&primary, &scratch).await;
    let after = inventory().await;
    inherited_model_refusal(&primary, &scratch).await;
    assert!(outcome.is_ok(), "{}", why(&outcome));
    assert_eq!(after, before);
}

#[tokio::test]
#[ignore = "needs the owned SQL Server resolver fixtures"]
async fn mssql_supplied_guest_admission_dirty_inventory_and_persistent_options_refuse_by_name() {
    let primary = server("PBPS_TEST_DB");
    let (scratch_name, scratch) = create_database(&primary).await;
    let (guest_name, guest) = create_database(&primary).await;
    connection(&guest)
        .await
        .execute("GRANT CONNECT TO guest;")
        .await
        .unwrap();
    let guest_refusal = refusal(&primary, &scratch).await.to_string();
    connection(&guest)
        .await
        .execute("REVOKE CONNECT FROM guest;")
        .await
        .unwrap();
    connection(&scratch)
        .await
        .execute("CREATE TABLE dbo.left_behind(i int);")
        .await
        .unwrap();
    let dirty = refusal(&primary, &scratch).await.to_string();
    connection(&scratch)
        .await
        .execute("DROP TABLE dbo.left_behind;")
        .await
        .unwrap();
    connection(&scratch)
        .await
        .execute(&format!(
            "ALTER DATABASE [{scratch_name}] SET AUTO_SHRINK ON;"
        ))
        .await
        .unwrap();
    let options = refusal(&primary, &scratch).await.to_string();
    drop_database(&primary, &scratch_name).await;
    drop_database(&primary, &guest_name).await;
    assert!(guest_refusal.contains(&guest_name), "{guest_refusal}");
    assert!(dirty.contains("left_behind"), "{dirty}");
    assert!(options.contains("is_auto_shrink_on"), "{options}");
}

#[tokio::test]
#[ignore = "needs Docker and the owned SQL Server resolver fixtures"]
async fn mssql_docker_removes_the_container_on_success_failure_and_cancellation() {
    let target = server("PBPS_TEST_DB");
    let image = server("PBPS_TEST_RESOLVER_MSSQL_IMAGE");
    let identity = sql::cluster_identity(&mut connection(&target).await)
        .await
        .unwrap();
    for fails in [false, true] {
        let name = std::sync::Arc::new(tokio::sync::Mutex::new(String::new()));
        let observed = name.clone();
        let result = super::container::with_container(
            Driver::Mssql,
            &target,
            &identity,
            &image,
            pbps_config::resolver::PullPolicy::Never,
            async move |url: &str| {
                let mut conn = connection(url).await;
                let rows = conn
                    .query("SELECT CONVERT(nvarchar(128),SERVERPROPERTY('ServerName')) AS name;")
                    .await
                    .unwrap();
                let server = rows[0].try_get::<&str>("name").unwrap().unwrap();
                // Docker's default hostname is the container ID prefix.
                *observed.lock().await = server.to_owned();
                if fails {
                    Err(ProduceError::Run(Error::Binding(
                        "intentional fixture refusal".into(),
                    )))
                } else {
                    Ok(())
                }
            },
        )
        .await;
        assert_eq!(result.is_err(), fails);
        let id = name.lock().await.clone();
        let inspect = tokio::process::Command::new("docker")
            .args(["inspect", &id])
            .output()
            .await
            .unwrap();
        assert!(!inspect.status.success(), "container {id} remained");
    }
    let (sender, receiver) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(async move {
        super::container::with_container(
            Driver::Mssql,
            &target,
            &identity,
            &image,
            pbps_config::resolver::PullPolicy::Never,
            async move |url: &str| {
                let rows = connection(url)
                    .await
                    .query("SELECT CONVERT(nvarchar(128),SERVERPROPERTY('ServerName')) AS name;")
                    .await
                    .unwrap();
                sender
                    .send(rows[0].try_get::<&str>("name").unwrap().unwrap().to_owned())
                    .unwrap();
                std::future::pending::<Result<(), ProduceError>>().await
            },
        )
        .await
    });
    let id = receiver.await.unwrap();
    task.abort();
    let _ = task.await;
    let mut removed = false;
    for _ in 0..100 {
        let inspect = tokio::process::Command::new("docker")
            .args(["inspect", &id])
            .output()
            .await
            .unwrap();
        if !inspect.status.success() {
            removed = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert!(removed, "cancelled container {id} remained");
}

#[tokio::test]
#[ignore = "needs the owned SQL Server resolver fixtures"]
async fn mssql_supplied_independent_catalogs_refuse_untouched_and_cleanup_restores_fresh() {
    let target = server("PBPS_TEST_DB");
    let scratch = server("PBPS_TEST_MSSQL_SCRATCH_DB");
    let (name, on) = create_database(&scratch).await;
    let cases = catalog_cases::cases();
    let login = format!("pbps2236_owner_{:032x}", rand::random::<u128>());
    let password = sql::password(&format!("{:032x}", rand::random::<u128>()));
    let mut admin = connection(&scratch).await;
    admin
        .execute(&format!(
            "CREATE LOGIN [{login}] WITH PASSWORD=N'{password}',CHECK_POLICY=ON; \
         ALTER AUTHORIZATION ON DATABASE::[{name}] TO [{login}];",
        ))
        .await
        .unwrap();
    let supplied = format!("{on};User Id={login};Password={password}");
    let mut conn = connection(&supplied).await;
    let result: Result<(), String> = async {
        if sql::provisions(&mut conn)
            .await
            .map_err(|error| error.to_string())?
        {
            return Err("the fixture owner unexpectedly has instance creation privileges".into());
        }
        for (expected, create) in cases.restore {
            conn.execute(&create)
                .await
                .map_err(|error| error.to_string())?;
            let refused = refusal(&target, &supplied).await.to_string();
            let untouched = sql::inventory::read(&mut conn, None)
                .await
                .map_err(|error| error.to_string())?
                .names();
            let cleanup = sql::inventory::cleanup(&mut conn, None).await;
            let fresh = sql::inventory::read(&mut conn, None)
                .await
                .map_err(|error| error.to_string())?;
            if !refused.contains(&expected)
                || !untouched.iter().any(|item| item.contains(&expected))
                || cleanup.is_err()
                || !fresh.is_empty()
            {
                return Err(format!(
                    "{expected}: refusal={refused}; untouched={untouched:?}; \
                     cleanup={cleanup:?}; remaining={:?}",
                    fresh.names(),
                ));
            }
        }
        Ok(())
    }
    .await;
    drop(conn);
    drop_database(&scratch, &name).await;
    admin
        .execute(&format!("DROP LOGIN [{login}];"))
        .await
        .unwrap();
    result.unwrap();
}

#[tokio::test]
#[ignore = "needs the owned SQL Server resolver fixtures"]
async fn mssql_supplied_built_in_refusals_preserve_state_and_residuals_reach_rehearsal() {
    let target = server("PBPS_TEST_DB");
    let scratch = server("PBPS_TEST_MSSQL_SCRATCH_DB");
    let cases = catalog_cases::cases();
    let login = format!("pbps2263_owner_{:032x}", rand::random::<u128>());
    let password = sql::password(&format!("{:032x}", rand::random::<u128>()));
    let mut admin = connection(&scratch).await;
    admin
        .execute(&format!(
            "CREATE LOGIN [{login}] WITH PASSWORD=N'{password}',CHECK_POLICY=ON;"
        ))
        .await
        .unwrap();
    let mut results = Vec::new();
    for (refuses, (expected, create)) in cases
        .refuse
        .into_iter()
        .map(|case| (true, case))
        .chain(cases.residual.into_iter().map(|case| (false, case)))
    {
        let (name, on) = create_database(&scratch).await;
        admin
            .execute(&format!(
                "ALTER AUTHORIZATION ON DATABASE::[{name}] TO [{login}];"
            ))
            .await
            .unwrap();
        let supplied = format!("{on};User Id={login};Password={password}");
        let mut conn = connection(&supplied).await;
        let result: Result<(), String> = async {
            conn.execute(&create).await.map_err(|error| error.to_string())?;
            let before = conn.query(catalog_cases::BUILTIN_STATE).await
                .map_err(|error| error.to_string())?;
            let before = before[0].try_get::<&str>("state").unwrap().unwrap().to_owned();
            let outcome = outcome(&target, &supplied).await;
            let refused = why(&outcome);
            let names = sql::inventory::read(&mut conn, None).await
                .map_err(|error| error.to_string())?.names();
            let cleanup = sql::inventory::cleanup(&mut conn, None).await;
            let remaining = sql::inventory::read(&mut conn, None).await
                .map_err(|error| error.to_string())?.names();
            let after = conn.query(catalog_cases::BUILTIN_STATE).await
                .map_err(|error| error.to_string())?;
            let after = after[0].try_get::<&str>("state").unwrap().unwrap();
            let valid = if refuses {
                refused.contains(&expected)
                    && names.iter().any(|name| name.contains(&expected))
                    && cleanup.as_ref().is_err_and(|errors| errors.iter().any(|error|
                        error.contains(&expected) && error.contains(
                            "this database cannot be restored to a fresh state; supply a new scratch database"
                        )))
                    && remaining == names && before == after
            } else {
                outcome.is_ok() && names.is_empty() && cleanup.is_ok()
                    && remaining.is_empty() && before == after
            };
            if !valid {
                return Err(format!("{expected}: refusal={refused}; names={names:?}; cleanup={cleanup:?}; remaining={remaining:?}; unchanged={}", before == after));
            }
            Ok(())
        }.await;
        drop(conn);
        // An owned database is the only measured way to discard changed sys
        // ownership; perform it before asserting and keep the login until last.
        drop_database(&scratch, &name).await;
        let absent = admin
            .query(&format!(
                "SELECT name FROM sys.databases WHERE name=N'{name}';"
            ))
            .await
            .unwrap()
            .is_empty();
        results.push((name, absent, result));
    }
    admin
        .execute(&format!("DROP LOGIN [{login}];"))
        .await
        .unwrap();
    for (name, absent, result) in results {
        assert!(absent, "fixture database {name} remained");
        result.unwrap();
    }
}

#[tokio::test]
#[ignore = "needs the owned SQL Server resolver fixtures"]
async fn mssql_externals_file_failure_and_partial_commit_release_owned_resources() {
    let target_server = server("PBPS_TEST_DB");
    let scratch_server = server("PBPS_TEST_MSSQL_SCRATCH_DB");
    let (target_database, target) = create_database(&target_server).await;
    for (script, needle) in [
        ("CREATE TABLE dbo.absent2269(i int);", "absent2269"),
        (
            "BEGIN TRAN; CREATE TABLE dbo.a2269(i int); EXEC(N'COMMIT; CREATE TABLE dbo.b2269(i int); THROW 51001,''committed2269'',1;');",
            "committed2269",
        ),
        (
            "CREATE TABLE dbo.c2269(i int);\nGO\nTHROW 51001,'batch2269',1;",
            "batch2269",
        ),
        (":r never_run.sql", "sqlcmd"),
    ] {
        let outcome = Box::pin(refusal_file_inner(&target, &scratch_server, Some(script))).await;
        assert!(
            matches!(&outcome, ProduceError::Run(Error::Externals(reasons)) if reasons.iter().any(|r| r.contains(needle))),
            "{outcome}"
        );
        let mut admin = connection(&scratch_server).await;
        let rows = admin
            .query("SELECT name FROM sys.databases WHERE name LIKE N'pbps[_]scratch[_]%';")
            .await
            .unwrap();
        assert!(rows.is_empty(), "owned database leaked");
        let rows = admin
            .query("SELECT name FROM sys.server_principals WHERE name LIKE N'pbps[_]run[_]%';")
            .await
            .unwrap();
        assert!(rows.is_empty(), "owned login leaked");
    }
    drop_database(&target_server, &target_database).await;
}

#[tokio::test]
#[ignore = "needs the owned SQL Server resolver fixtures"]
async fn mssql_externals_file_is_compared_then_reaches_reconstruction_refusal() {
    let target_server = server("PBPS_TEST_DB");
    let scratch_server = server("PBPS_TEST_MSSQL_SCRATCH_DB");
    let (name, target) = create_database(&target_server).await;
    connection(&target)
        .await
        .execute("CREATE SCHEMA unused2269 AUTHORIZATION dbo;")
        .await
        .unwrap();
    connection(&target).await.execute("GRANT SELECT ON SCHEMA::unused2269 TO public; CREATE TABLE unused2269.external2269(i int CONSTRAINT external2269_pk PRIMARY KEY); GRANT SELECT ON unused2269.external2269 TO public;").await.unwrap();
    let outcome = Box::pin(outcome_file_inner(
        &target,
        &scratch_server,
        Some("CREATE SCHEMA unused2269 AUTHORIZATION dbo;\nGO\nCREATE TABLE unused2269.external2269(i int CONSTRAINT external2269_pk PRIMARY KEY); GRANT SELECT ON unused2269.external2269 TO public;"),
    ))
    .await;
    let extra_acl = Box::pin(refusal_file_inner(&target,&scratch_server,Some("CREATE SCHEMA unused2269 AUTHORIZATION dbo;\nGO\nCREATE TABLE unused2269.external2269(i int CONSTRAINT external2269_pk PRIMARY KEY); GRANT INSERT ON unused2269.external2269 TO public;"))).await;
    drop_database(&target_server, &name).await;
    assert!(outcome.is_ok(), "{}", why(&outcome));
    assert!(
        matches!(&extra_acl,ProduceError::Run(Error::Externals(findings)) if findings.iter().any(|f| f.contains("staged:permissions"))),
        "{extra_acl}"
    );
}

#[tokio::test]
#[ignore = "needs the owned SQL Server resolver fixtures"]
async fn mssql_prepare_renders_missing_external_and_returns_entered_compile_connection() {
    use pbps_mssql::resolver::capture;
    let target_server = server("PBPS_TEST_DB");
    let scratch_server = server("PBPS_TEST_MSSQL_SCRATCH_DB");
    let (name, target_url) = create_database(&target_server).await;
    let result=tokio::spawn(async move {
        let mut target=connection(&target_url).await;
        target.execute("CREATE SCHEMA ext2269 AUTHORIZATION dbo;").await.unwrap();
        target.execute("CREATE TYPE ext2269.amount FROM decimal(9,2) NOT NULL;").await.unwrap();
        target.execute("CREATE TABLE dbo.ext2269(i ext2269.amount CONSTRAINT ext2269_pk PRIMARY KEY);").await.unwrap();
        target.execute("CREATE VIEW dbo.managed2269 AS SELECT i FROM dbo.ext2269;").await.unwrap();
        let mut schema=Schema::default();
        schema.modules.insert("dbo.managed2269".parse().unwrap(),pbps_model::Module { kind:pbps_model::ModuleKind::View,description:None,options: Default::default(),definition:"CREATE VIEW dbo.managed2269 AS SELECT i FROM dbo.ext2269;".into() });
        let ids=IdsFile::default();
        let side=pbps_diff::Side { schema:&schema,ids:&ids };
        let bootstrap=pbps_diff::resolver::ordinary(side,side,&pbps_mssql::Mssql,&Hints::default()).unwrap();
        let binding=BindingRequest { dependencies: &Default::default(), bootstrap:&bootstrap,base:&schema,desired:&schema };
        let request=super::super::ScopeRequest { schemas:vec!["dbo".into()],..Default::default() };
        for file in [None,Some("CREATE SCHEMA ext2269 AUTHORIZATION dbo;\nGO\nCREATE TYPE ext2269.amount FROM decimal(9,2) NOT NULL;\nGO\nCREATE TABLE dbo.ext2269(i ext2269.amount CONSTRAINT ext2269_pk PRIMARY KEY);")] {
        let tokens=super::Tokens::generate();
        let provenance=provenance();
        let identity=sql::cluster_identity(&mut target).await.unwrap();
        let mut admin=connection(&scratch_server).await;
        let (provisioning,mut state)=super::engine::scratch(Driver::Mssql,&mut target,&mut admin,&tokens,&identity,&scratch_server,&request,&Standard::Mssql,file,&provenance).await.unwrap();
        let (catalog,authorization)=crate::resolver::scope::read_target(&mut target,Driver::Mssql,&request.schemas,&[],&request.schemas,&[]).await.unwrap();
        let prepared=state.prepare(provisioning,&mut admin,&mut target,&catalog,&authorization,&request.schemas,&binding,&[], Some(&crate::resolver::server::scratch::mssql_live_tests::capture_key())).await;
        let verification=tokio::spawn(async move { match prepared {
            Ok(super::engine::Prepared::Mssql(mut prepared)) => {
                assert!(matches!(prepared.map,crate::resolver::scope::Principals::Mssql(_)));
                assert!(format!("{:?}",prepared.inventory).contains("ext2269"));
                assert!(prepared.staged.compared().contains(&capture::object("dbo","ext2269")));
                let rows=prepared.compile.query("SELECT USER_NAME() AS principal,ISNULL(OBJECT_ID(N'dbo.ext2269'),0) AS ext;").await.unwrap();
                assert_eq!(rows[0].try_get::<&str>("principal").unwrap(),Some("dbo"));
                assert_ne!(rows[0].try_get::<i32>("ext").unwrap(),Some(0));
                assert!(prepared.compile.execute("REVERT;").await.is_err());
                Ok(())
            },
            Ok(super::engine::Prepared::Postgres(_)) => panic!("wrong engine"),
            Err(error) => Err(error),
        }}).await;
        let cleanup=state.release(&mut admin).await;
        cleanup.unwrap();
        verification.map_err(|error| Error::Read(error.to_string()))??;
        }
        Ok::<(),Error>(())
    }).await;
    drop_database(&target_server, &name).await;
    result.unwrap().unwrap();
}

#[tokio::test]
#[ignore = "needs the owned SQL Server resolver fixtures"]
async fn mssql_supplied_type_permissions_release_to_empty_inventory() {
    let target_server = server("PBPS_TEST_DB");
    let scratch_server = target_server.clone();
    let (target_name, target_url) = create_database(&target_server).await;
    let (scratch_name, scratch_url) = create_database(&scratch_server).await;
    let result = tokio::spawn(async move {
        let mut target = connection(&target_url).await;
        target.execute("CREATE USER type_owner WITHOUT LOGIN; CREATE USER type_grantor WITHOUT LOGIN; CREATE USER type_denied WITHOUT LOGIN;").await.unwrap();
        target.execute("CREATE SCHEMA ext2269 AUTHORIZATION dbo;").await.unwrap();
        target.execute("CREATE TYPE ext2269.granted FROM int NULL; CREATE TYPE ext2269.denied FROM int NULL;").await.unwrap();
        target.execute("ALTER AUTHORIZATION ON TYPE::ext2269.granted TO type_owner; ALTER AUTHORIZATION ON TYPE::ext2269.denied TO type_owner;").await.unwrap();
        target.execute("GRANT REFERENCES ON TYPE::ext2269.granted TO type_grantor WITH GRANT OPTION AS type_owner; GRANT REFERENCES ON TYPE::ext2269.granted TO public AS type_grantor; DENY REFERENCES ON TYPE::ext2269.denied TO type_denied AS type_owner;").await.unwrap();
        target.execute("CREATE TABLE dbo.external2269(g ext2269.granted,d ext2269.denied);").await.unwrap();
        target.execute("CREATE VIEW dbo.managed2269 AS SELECT g,d FROM dbo.external2269;").await.unwrap();
        let mut schema = Schema::default();
        schema.modules.insert("dbo.managed2269".parse().unwrap(), pbps_model::Module {
            kind: pbps_model::ModuleKind::View,
            description: None,
            definition: "CREATE VIEW dbo.managed2269 AS SELECT g,d FROM dbo.external2269;".into(),
            options: Default::default(),
        });
        let ids = IdsFile::default();
        let side = pbps_diff::Side { schema: &schema, ids: &ids };
        let bootstrap = pbps_diff::resolver::ordinary(side, side, &pbps_mssql::Mssql, &Hints::default()).unwrap();
        let binding = BindingRequest { dependencies: &Default::default(), bootstrap: &bootstrap, base: &schema, desired: &schema };
        let request = super::super::ScopeRequest { schemas: vec!["dbo".into()], ..Default::default() };
        let tokens = super::Tokens::generate();
        let provenance = provenance();
        let identity = sql::cluster_identity(&mut target).await.unwrap();
        let mut admin = connection(&scratch_url).await;
        let (provisioning, mut state) = super::engine::scratch(
            Driver::Mssql, &mut target, &mut admin, &tokens, &identity, &scratch_url,
            &request, &Standard::Mssql, None, &provenance,
        ).await.unwrap();
        assert_eq!(provisioning, super::Provisioning::Supplied);
        let (catalog, authorization) = crate::resolver::scope::read_target(
            &mut target, Driver::Mssql, &request.schemas, &[], &request.schemas, &[],
        ).await.unwrap();
        let prepared = state.prepare(
            provisioning, &mut admin, &mut target, &catalog, &authorization,
            &request.schemas, &binding, &[], Some(&crate::resolver::server::scratch::mssql_live_tests::capture_key()),
        ).await.unwrap();
        let super::engine::Prepared::Mssql(mut prepared) = prepared else { panic!("wrong engine"); };
        let rows = prepared.compile.query("SELECT p.state_desc,USER_NAME(p.grantee_principal_id) AS grantee,USER_NAME(p.grantor_principal_id) AS grantor,USER_NAME(t.principal_id) AS owner FROM sys.database_permissions p JOIN sys.types t ON p.class=6 AND t.user_type_id=p.major_id;").await.unwrap();
        assert_eq!(rows.len(), 3);
        assert!(rows.iter().any(|r| r.try_get::<&str>("state_desc").unwrap()==Some("GRANT")));
        assert!(rows.iter().any(|r| r.try_get::<&str>("state_desc").unwrap()==Some("DENY")));
        assert!(rows.iter().all(|r| r.try_get::<&str>("owner").unwrap()!=Some("dbo")));
        // Drop compile sessions before releasing the supplied database through
        // the real fresh-session sandbox cleanup path.
        drop(prepared);
        state.release(&mut admin).await.unwrap();
        assert!(sql::inventory::read(&mut admin, None).await.unwrap().is_empty());
    }).await;
    // Even a regression panic must release both owned fixtures and their users.
    drop_database(&target_server, &target_name).await;
    drop_database(&scratch_server, &scratch_name).await;
    result.unwrap();
}

/// Case 14: a reconnect that reaches a backend differing in a compared fact
/// refuses, naming the fact; one that reaches an equal backend does not.
#[tokio::test]
#[ignore = "needs the owned SQL Server resolver fixtures"]
async fn mssql_a_reconnect_to_a_differing_backend_refuses_and_an_equal_one_does_not() {
    let target_server = server("PBPS_TEST_DB");
    let scratch_server = target_server.clone();
    let (target_name, target_url) = create_database(&target_server).await;
    let (scratch_name, scratch_url) = create_database(&scratch_server).await;
    let scratch_database = scratch_name.clone();
    let result = tokio::spawn(async move {
        let mut target = connection(&target_url).await;
        target
            .execute("CREATE TABLE dbo.t2143(id int NOT NULL);")
            .await
            .unwrap();
        let mut schema = Schema::default();
        schema.modules.insert(
            "dbo.v2143".parse().unwrap(),
            pbps_model::Module {
                kind: pbps_model::ModuleKind::View,
                description: None,
                definition: "CREATE VIEW dbo.v2143 AS SELECT id FROM dbo.t2143;".into(),
                options: Default::default(),
            },
        );
        let ids = IdsFile::default();
        let side = pbps_diff::Side {
            schema: &schema,
            ids: &ids,
        };
        let bootstrap =
            pbps_diff::resolver::ordinary(side, side, &pbps_mssql::Mssql, &Hints::default())
                .unwrap();
        let binding = BindingRequest {
            dependencies: &Default::default(),
            bootstrap: &bootstrap,
            base: &schema,
            desired: &schema,
        };
        let request = super::super::ScopeRequest {
            schemas: vec!["dbo".into()],
            ..Default::default()
        };
        let tokens = super::Tokens::generate();
        let provenance = provenance();
        let identity = sql::cluster_identity(&mut target).await.unwrap();
        let mut admin = connection(&scratch_url).await;
        let (provisioning, mut state) = super::engine::scratch(
            Driver::Mssql,
            &mut target,
            &mut admin,
            &tokens,
            &identity,
            &scratch_url,
            &request,
            &Standard::Mssql,
            None,
            &provenance,
        )
        .await
        .unwrap();
        let (catalog, authorization) = crate::resolver::scope::read_target(
            &mut target,
            Driver::Mssql,
            &request.schemas,
            &[],
            &request.schemas,
            &[],
        )
        .await
        .unwrap();
        let prepared = state
            .prepare(
                provisioning,
                &mut admin,
                &mut target,
                &catalog,
                &authorization,
                &request.schemas,
                &binding,
                &[],
                Some(&crate::resolver::server::scratch::mssql_live_tests::capture_key()),
            )
            .await
            .unwrap();
        let super::engine::Prepared::Mssql(prepared) = prepared else {
            panic!("wrong engine");
        };
        let reach = prepared.reach();
        let option = |on: bool| {
            format!(
                "ALTER DATABASE [{scratch_database}] SET ANSI_PADDING {}",
                if on { "ON" } else { "OFF" }
            )
        };
        let equal = reach
            .open()
            .await
            .map(|_| ())
            .map_err(|error| error.to_string());
        admin.execute(&option(true)).await.unwrap();
        let differing = reach
            .open()
            .await
            .map(|_| ())
            .map_err(|error| error.to_string());
        admin.execute(&option(false)).await.unwrap();
        let restored = reach
            .open()
            .await
            .map(|_| ())
            .map_err(|error| error.to_string());
        drop(prepared);
        state.release(&mut admin).await.unwrap();
        assert!(equal.is_ok(), "{equal:?}");
        let refusal = differing.expect_err("a differing backend must refuse");
        assert!(refusal.contains("database_ansi_padding"), "{refusal}");
        assert!(restored.is_ok(), "{restored:?}");
    })
    .await;
    drop_database(&target_server, &target_name).await;
    drop_database(&scratch_server, &scratch_name).await;
    result.unwrap();
}

#[path = "same_instance2270_tests.rs"]
mod same_instance2270;

#[path = "dependencies2323_tests.rs"]
mod dependencies2323;

#[path = "reconstruct2374_tests.rs"]
mod reconstruct2374;

#[path = "roundtrip2143_tests.rs"]
mod roundtrip2143;

#[path = "recovery2410_mssql_tests.rs"]
mod recovery2410;

#[path = "docker2410_mssql_tests.rs"]
mod docker2410;

#[path = "visibility2314_tests.rs"]
mod visibility2314;

#[cfg(unix)]
pub(super) fn capture_key() -> pbps_db::fingerprint::EnvironmentFingerprintKey {
    capture_key_from(b"AQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQE=")
}

pub(super) fn capture_key_from(text: &[u8]) -> pbps_db::fingerprint::EnvironmentFingerprintKey {
    use std::io::Write as _;
    #[cfg(unix)]
    use std::os::unix::fs::OpenOptionsExt as _;
    let path = std::env::temp_dir().join(format!(
        "pbps2375-key-{}-{}",
        std::process::id(),
        rand::random::<u64>()
    ));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut file = options.open(&path).unwrap();
    file.write_all(text).unwrap();
    drop(file);
    let key = pbps_db::fingerprint::EnvironmentFingerprintKey::from_file(&path).unwrap();
    std::fs::remove_file(path).unwrap();
    key
}

#[path = "assessment2375_tests.rs"]
mod assessment2375;

#[cfg(not(unix))]
pub(super) fn capture_key() -> pbps_db::fingerprint::EnvironmentFingerprintKey {
    pbps_db::fingerprint::EnvironmentFingerprintKey::from_env("PBPS_TEST_FINGERPRINT_KEY")
        .expect("the LocalDB runner supplies the environment key")
}

#[path = "chained2376_tests.rs"]
mod chained2376;

#[path = "chained2541_tests.rs"]
mod chained2541;

#[path = "warm2585_tests.rs"]
mod warm2585;
