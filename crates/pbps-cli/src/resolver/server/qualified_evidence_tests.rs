//! Connected #1274 producer oracles on native PostgreSQL fixtures.
//!
//! The target is seeded separately. The producer owns a fresh ScratchRun and
//! receives the ordinary empty-to-desired bootstrap; no test prepares its
//! scratch schema by hand. Connected cases require the pinned native container
//! or supplied-server fixture; UID transition regressions are pure.

use super::qualified_evidence_cases as cases;
use super::*;
use crate::resolver::docker::CandidateSession;
use pbps_config::resolver::{PullPolicy, ResolverProfile};
use pbps_db::fingerprint::FingerprintKey;
use pbps_db::transport::PeerVerifiedConn;
use pbps_model::resolver::{ObjectOwnership, PlanAnalysis, Surface};
use pbps_model::{Change, ChangeSet, Hints, IdsFile, PlanBaseline, PlanOrigin, SavedPlan, Schema};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::fs::OpenOptions;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

const ENVIRONMENT: &str = "fixture";

#[derive(Clone, Copy)]
enum Profile {
    Container,
    Supplied,
}

struct ProjectKey {
    root: PathBuf,
    project: pbps_config::Project,
}

impl ProjectKey {
    fn new(with_key: bool) -> Self {
        let root = std::env::temp_dir().join(format!(
            "pbps-1274-evidence-{:032x}",
            rand::random::<u128>()
        ));
        std::fs::create_dir(&root).unwrap();
        if with_key {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(root.join("key"))
                .unwrap();
            writeln!(file, "{}", FingerprintKey::generate()).unwrap();
        }
        let config = format!(
            "dialect: postgres\nenvironments:\n  {ENVIRONMENT}:\n    url_env: PBPS_1274_UNUSED\n{}",
            if with_key {
                "    fingerprint_key_file: key\n"
            } else {
                ""
            }
        );
        let path = root.join("pbps.yml");
        std::fs::write(&path, &config).unwrap();
        let parsed = pbps_config::Config::parse(&config, &path).unwrap();
        let project = pbps_config::Project {
            root: root.clone(),
            config: parsed,
        };
        Self { root, project }
    }
}

impl Drop for ProjectKey {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.root).unwrap();
    }
}

fn ids(schema: &Schema, previous: &IdsFile) -> IdsFile {
    pbps_diff::resolve(
        schema,
        previous,
        &[],
        &pbps_diff::Context {
            operator: "1274-test".into(),
            today: "2026-09-29".into(),
        },
    )
    .unwrap()
    .ids
}

struct Inputs {
    base: Schema,
    desired: Schema,
    base_ids: IdsFile,
    desired_ids: IdsFile,
    bootstrap: Vec<Change>,
    hints: Hints,
}

impl Inputs {
    fn overload() -> Self {
        Self::from_pair(cases::pair_with_cross_kind_surfaces())
    }

    fn from_pair((base, desired): (Schema, Schema)) -> Self {
        Self::from_pair_with_extras((base, desired), &[])
    }

    fn from_pair_with_extras((base, desired): (Schema, Schema), extras: &[String]) -> Self {
        let base_ids = ids(&base, &IdsFile::default());
        let desired_ids = ids(&desired, &base_ids);
        Self::with_ids_and_extras(base, desired, base_ids, desired_ids, extras)
    }

    fn with_ids(base: Schema, desired: Schema, base_ids: IdsFile, desired_ids: IdsFile) -> Self {
        Self::with_ids_and_extras(base, desired, base_ids, desired_ids, &[])
    }

    fn with_ids_and_extras(
        base: Schema,
        desired: Schema,
        base_ids: IdsFile,
        desired_ids: IdsFile,
        extras: &[String],
    ) -> Self {
        let empty = Schema::default();
        let dialect = pbps_pg::Postgres::with_write_path_extras(extras.to_vec());
        let bootstrap = pbps_diff::diff(
            pbps_diff::Side {
                schema: &empty,
                ids: &IdsFile::default(),
            },
            pbps_diff::Side {
                schema: &desired,
                ids: &desired_ids,
            },
            &dialect,
            &Hints::default(),
        )
        .unwrap()
        .changes
        .into_iter()
        .map(|planned| planned.change)
        .collect();
        Self {
            base,
            desired,
            base_ids,
            desired_ids,
            bootstrap,
            hints: Hints::default(),
        }
    }

    fn binding(&self) -> BindingRequest<'_> {
        BindingRequest {
            bootstrap: &self.bootstrap,
            desired: &self.desired,
            base: &self.base,
        }
    }

    fn base(&self) -> pbps_diff::Side<'_> {
        pbps_diff::Side {
            schema: &self.base,
            ids: &self.base_ids,
        }
    }

    fn desired(&self) -> pbps_diff::Side<'_> {
        pbps_diff::Side {
            schema: &self.desired,
            ids: &self.desired_ids,
        }
    }
}

fn docker(args: &[&str]) -> String {
    let binary = std::env::var("PBPS_NATIVE_DOCKER").unwrap();
    assert!(Path::new(&binary).is_absolute());
    let socket = std::env::var("PBPS_RESOLVER_TEST_SOCKET").unwrap();
    let output = Command::new(binary)
        .arg("--host")
        .arg(format!("unix://{socket}"))
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "fixture Docker observation {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

fn containers() -> BTreeMap<String, String> {
    docker(&[
        "ps",
        "-a",
        "--no-trunc",
        "--filter",
        "name=pbps-resolver-",
        "--format",
        "{{json .}}",
    ])
    .lines()
    .map(|row| {
        let row: Value = serde_json::from_str(row).unwrap();
        (
            row["ID"].as_str().unwrap().to_owned(),
            row["Names"].as_str().unwrap().to_owned(),
        )
    })
    .collect()
}

/// This is an environmental observation, not an ownership claim. The
/// product's returned names are the only authority for a surviving resource.
/// In particular, a concurrently created pbps-resolver container is never
/// removed merely because it appeared after this snapshot.
struct ObservedContainers(BTreeMap<String, String>);

impl ObservedContainers {
    fn begin() -> Self {
        Self(containers())
    }

    fn finish(&self, recovery_names: &[String]) {
        let newly_seen: BTreeMap<_, _> = containers()
            .into_iter()
            .filter(|(id, _)| !self.0.contains_key(id))
            .collect();
        let mut unowned = Vec::new();
        for (id, name) in &newly_seen {
            let inspected: Value =
                serde_json::from_str::<Value>(&docker(&["inspect", "--type", "container", id]))
                    .unwrap()[0]
                    .clone();
            assert_eq!(inspected["Id"].as_str(), Some(id.as_str()));
            assert_eq!(
                inspected["Name"].as_str(),
                Some(format!("/{name}").as_str())
            );
            if !recovery_names.contains(name) {
                unowned.push((id.clone(), name.clone()));
            }
        }
        assert!(
            unowned.is_empty(),
            "newly seen containers have no exact run-owned recovery name; no container was deleted: {unowned:?}"
        );
        eprintln!("#1274 named surviving containers (left untouched): {newly_seen:?}");
    }
}

async fn setup(statements: &[&str]) {
    let mut peer = PeerVerifiedConn::connect(
        Driver::Postgres,
        &std::env::var("PBPS_NATIVE_CONNECTION").unwrap(),
    )
    .await
    .unwrap();
    peer.query(cases::RESET).await.unwrap();
    peer.query(cases::EXTRA_RESET).await.unwrap();
    for sql in statements {
        peer.query(sql).await.unwrap();
    }
}

async fn table_column_grants(table: &str, column: &str) -> (String, String, String) {
    // Both inputs are fixed names from this disposable test fixture.
    assert!(matches!(table, "t" | "u"));
    assert!(matches!(column, "id" | "n"));
    let mut peer = PeerVerifiedConn::connect(
        Driver::Postgres,
        &std::env::var("PBPS_NATIVE_CONNECTION").unwrap(),
    )
    .await
    .unwrap();
    let rows = peer
        .query(&format!(
            "SELECT pg_catalog.pg_get_userbyid(c.relowner)::text AS owner, \
             c.relacl::text AS table_acl, a.attacl::text AS column_acl \
             FROM pg_catalog.pg_class c \
             JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
             JOIN pg_catalog.pg_attribute a ON a.attrelid = c.oid \
             WHERE n.nspname = 'pbps_evidence1274' AND c.relname = '{table}' \
               AND a.attname = '{column}'"
        ))
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    let owner = rows[0]
        .try_get::<&str>("owner")
        .unwrap()
        .unwrap()
        .to_owned();
    let table_acl = rows[0]
        .try_get::<&str>("table_acl")
        .unwrap()
        .unwrap()
        .to_owned();
    let column_acl = rows[0]
        .try_get::<&str>("column_acl")
        .unwrap()
        .unwrap()
        .to_owned();
    assert!(table_acl.contains("pg_monitor"));
    assert!(column_acl.contains("pg_monitor"));
    (owner, table_acl, column_acl)
}

/// An ordinary table owner has a shared edge; a pinned owner and a table-owned
/// index do not. Keep this catalog oracle beside the complete sealed inventory.
async fn catalog_owner_dependency(subject: &str) -> (String, i64, i64) {
    let (class, source) = match subject {
        "routine-a" => (
            "pg_proc",
            "SELECT oid, proowner AS owner FROM pg_catalog.pg_proc \
             WHERE oid = 'pbps_evidence1274.a()'::regprocedure",
        ),
        "table-t" => (
            "pg_class",
            "SELECT oid, relowner AS owner FROM pg_catalog.pg_class \
             WHERE oid = 'pbps_evidence1274.t'::regclass",
        ),
        "table-u" => (
            "pg_class",
            "SELECT oid, relowner AS owner FROM pg_catalog.pg_class \
             WHERE oid = 'pbps_evidence1274.u'::regclass",
        ),
        "index-ix" => (
            "pg_class",
            "SELECT oid, relowner AS owner FROM pg_catalog.pg_class \
             WHERE oid = 'pbps_evidence1274.ix'::regclass AND relkind = 'i'",
        ),
        _ => panic!("unsupported exact fixture subject {subject}"),
    };
    let mut peer = PeerVerifiedConn::connect(
        Driver::Postgres,
        &std::env::var("PBPS_NATIVE_CONNECTION").unwrap(),
    )
    .await
    .unwrap();
    let rows = peer
        .query(&format!(
            "SELECT pg_catalog.pg_get_userbyid(s.owner)::text AS owner, \
                    count(d.objid) AS owner_edges, \
                    count(d.objid) FILTER (WHERE d.refclassid = 'pg_catalog.pg_authid'::regclass \
                        AND d.refobjid = s.owner) AS matching_owner_edges \
               FROM ({source}) s \
               LEFT JOIN pg_catalog.pg_shdepend d \
                 ON d.dbid = (SELECT oid FROM pg_catalog.pg_database \
                               WHERE datname = current_database()) \
                AND d.classid = 'pg_catalog.{class}'::regclass \
                AND d.objid = s.oid AND d.objsubid = 0 AND d.deptype = 'o' \
              GROUP BY s.owner"
        ))
        .await
        .unwrap();
    assert_eq!(rows.len(), 1, "the exact fixture subject exists");
    let row = &rows[0];
    (
        row.try_get::<&str>("owner").unwrap().unwrap().to_owned(),
        row.try_get::<i64>("owner_edges").unwrap().unwrap(),
        row.try_get::<i64>("matching_owner_edges").unwrap().unwrap(),
    )
}

/// Query the engine's ACL rows, not its textual ACL spelling. `NULL` and an
/// explicit owner-only ACL are different closing properties even when they
/// grant the same owner permission.
async fn routine_execution_acl(
    routine: &str,
    reader: &str,
) -> (bool, Option<bool>, i64, i64, i64, i64, i64) {
    assert!(matches!(routine, "a" | "f"));
    assert!(matches!(reader, "pg_monitor" | "pbps_native_alt"));
    let mut peer = PeerVerifiedConn::connect(
        Driver::Postgres,
        &std::env::var("PBPS_NATIVE_CONNECTION").unwrap(),
    )
    .await
    .unwrap();
    let rows = peer
        .query(&format!(
            "SELECT p.proacl IS NULL AS acl_null, \
                    p.proacl = pg_catalog.acldefault('f'::\"char\", p.proowner) AS builtin_acl, \
                    (SELECT count(*) FROM pg_catalog.aclexplode(p.proacl) a \
                      WHERE a.grantee = 0 AND a.privilege_type = 'EXECUTE') AS public_execute, \
                    (SELECT count(*) FROM pg_catalog.aclexplode(p.proacl) a \
                      WHERE a.grantee = p.proowner AND a.grantor = p.proowner \
                        AND a.privilege_type = 'EXECUTE') AS owner_execute, \
                    (SELECT count(*) FROM pg_catalog.aclexplode(p.proacl) a \
                      WHERE a.grantee = '{reader}'::regrole AND a.grantor = p.proowner \
                        AND a.privilege_type = 'EXECUTE' AND NOT a.is_grantable) AS reader_execute, \
                    (SELECT count(*) FROM pg_catalog.aclexplode(p.proacl) a \
                      WHERE a.grantee = '{reader}'::regrole AND a.grantor = p.proowner \
                        AND a.privilege_type = 'EXECUTE' AND a.is_grantable) AS reader_option, \
                    (SELECT count(*) FROM pg_catalog.pg_shdepend d \
                      WHERE d.classid = 'pg_catalog.pg_proc'::regclass AND d.objid = p.oid \
                        AND d.deptype = 'a' AND d.refobjid = '{reader}'::regrole) AS reader_edge \
               FROM pg_catalog.pg_proc p \
              WHERE p.oid = 'pbps_evidence1274.{routine}()'::regprocedure",
        ))
        .await
        .unwrap();
    assert_eq!(rows.len(), 1, "the declared routine has one catalog row");
    let row = &rows[0];
    (
        row.try_get::<bool>("acl_null").unwrap().unwrap(),
        row.try_get::<bool>("builtin_acl").unwrap(),
        row.try_get::<i64>("public_execute").unwrap().unwrap(),
        row.try_get::<i64>("owner_execute").unwrap().unwrap(),
        row.try_get::<i64>("reader_execute").unwrap().unwrap(),
        row.try_get::<i64>("reader_edge").unwrap().unwrap(),
        row.try_get::<i64>("reader_option").unwrap().unwrap(),
    )
}

async fn target() -> NativeTarget {
    let peer = PeerVerifiedConn::connect(
        Driver::Postgres,
        &std::env::var("PBPS_NATIVE_CONNECTION").unwrap(),
    )
    .await
    .unwrap();
    NativeTarget::establish(
        peer,
        std::env::var("PBPS_NATIVE_SERVICE_PID")
            .unwrap()
            .parse()
            .unwrap(),
    )
    .await
    .unwrap()
}

async fn open(profile: Profile, target: &mut NativeTarget) -> ScratchRun {
    let recipe = target.database_recipe().await.unwrap();
    match profile {
        Profile::Container => {
            assert_eq!(
                std::env::var("PBPS_NATIVE_FACTORY_FIXTURE").as_deref(),
                Ok("1")
            );
            assert_eq!(std::env::var("PBPS_NATIVE_DRIVER").as_deref(), Ok("pg"));
            let image = std::env::var("PBPS_RESOLVER_TEST_IMAGE").unwrap();
            let socket = PathBuf::from(std::env::var("PBPS_RESOLVER_TEST_SOCKET").unwrap());
            let mut api = LocalApi::connect_native(&socket).await.unwrap();
            let acquired = api
                .acquire(&ResolverProfile::Docker {
                    image,
                    pull: PullPolicy::Never,
                })
                .await
                .unwrap();
            let mut candidate = CandidateSession::start(api, acquired, target)
                .await
                .unwrap();
            candidate.open_scratch(&recipe).await.unwrap()
        }
        Profile::Supplied => {
            assert_eq!(std::env::var("PBPS_SERVER_FIXTURE").as_deref(), Ok("1"));
            assert_eq!(std::env::var("PBPS_SERVER_DRIVER").as_deref(), Ok("pg"));
            let configured = std::env::var("PBPS_SERVER_ENDPOINT").unwrap();
            let mut refusals = Vec::new();
            for _ in 0..60 {
                let endpoint = ScratchEndpoint::parse(&configured).unwrap();
                let refused = match DedicatedServer::admit(endpoint, target).await {
                    Ok(mut server) => match server.open_scratch(&recipe).await {
                        Ok(run) => return run,
                        Err(refused) => {
                            server.discard().await.unwrap();
                            refused
                        }
                    },
                    Err(refused) => refused,
                };
                match refused {
                    ServerFailure {
                        cause:
                            cause @ (Error::Exclusivity(_) | Error::Containment(Premise::Occupants)),
                        recovery_names,
                    } if recovery_names.is_empty() => refusals.push(cause),
                    other => panic!("the supplied native fixture must open: {other}"),
                }
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            }
            panic!("the supplied native fixture did not become exclusive: {refusals:?}");
        }
    }
}

async fn close(run: &mut ScratchRun, owned: &mut Option<ObservedContainers>) {
    let closed = run.close().await;
    let recovery = closed
        .as_ref()
        .err()
        .map(|failure| failure.recovery_names.clone())
        .unwrap_or_default();
    if let Some(owned) = owned {
        owned.finish(&recovery);
    }
    closed.expect("a completed producer run removes its owned resources");
}

fn view<'a>(
    evidence: &'a pbps_model::resolver::ResolverEvidence,
    name: &str,
) -> &'a pbps_model::resolver::SurfaceResolution {
    let wanted = Surface::Module(format!("{}.{}", cases::SCHEMA, name).parse().unwrap());
    evidence
        .surfaces()
        .iter()
        .find(|surface| surface.surface == wanted)
        .expect("the declared view has a connected binding surface")
}

async fn positive(profile: Profile) {
    setup(cases::TARGET_SETUP).await;
    let mut owned = matches!(profile, Profile::Container).then(ObservedContainers::begin);
    let mut target = target().await;
    let mut run = open(profile, &mut target).await;
    let inputs = Inputs::overload();
    let key = ProjectKey::new(true);
    let result = run
        .plan_resolved(
            &mut target,
            &inputs.binding(),
            inputs.base(),
            inputs.desired(),
            &inputs.hints,
            &[],
            &key.project,
            Some(ENVIRONMENT),
        )
        .await;
    let result = match result {
        Ok(result) => result,
        Err(error) => {
            close(&mut run, &mut owned).await;
            panic!("the qualified fresh producer refused its compatible fixture: {error}");
        }
    };
    // The producer result is owned data. Release the run before any assertion
    // can panic, so an oracle failure cannot strand its exact owned container.
    close(&mut run, &mut owned).await;
    target.check().await.unwrap();
    setup(&[]).await;
    result.evidence.validate(&result.changes).unwrap();
    pbps_pg::resolver::validate_evidence(&result.evidence)
        .expect("the actual producer evidence must pass the artifact reader");

    let affected = view(&result.evidence, "v");
    let control = view(&result.evidence, "control");
    assert_ne!(
        affected.current.as_ref().unwrap().bindings,
        affected.desired.as_ref().unwrap().bindings,
        "the integer overload changes the unqualified call"
    );
    assert_eq!(
        control.current.as_ref().unwrap().bindings,
        control.desired.as_ref().unwrap().bindings,
        "the explicit numeric call remains on the old overload"
    );

    let affected_id: pbps_model::ModuleId = format!("{}.v", cases::SCHEMA).parse().unwrap();
    let control_id: pbps_model::ModuleId = format!("{}.control", cases::SCHEMA).parse().unwrap();
    let ordinary = pbps_diff::diff(
        inputs.base(),
        inputs.desired(),
        &pbps_pg::Postgres::with_write_path_extras(vec![]),
        &inputs.hints,
    )
    .unwrap();
    let ordinary_control: Vec<_> = ordinary
        .changes
        .iter()
        .filter(|step| {
            matches!(
                &step.change,
                Change::DropModule { id, .. }
                    | Change::CreateModule { id, .. }
                    | Change::AlterModule { id, .. }
                    if id == &control_id
            )
        })
        .collect();
    assert_eq!(ordinary_control.len(), 1);
    assert!(matches!(
        &ordinary_control[0].change,
        Change::AlterModule { id, .. } if id == &control_id
    ));
    let changes = &result.changes.changes;
    assert!(
        changes.iter().any(|step| {
            matches!(&step.change, Change::DropModule { id, .. } if id == &affected_id)
        }) && changes.iter().any(|step| {
            matches!(&step.change, Change::CreateModule { id, .. } if id == &affected_id)
        }),
        "the changed target binding must rebuild the affected view in the final typed sequence"
    );
    // ADR-0013 conservatively rebuilds lexical callers of an arriving
    // overload, even when the connected binding proves this call unchanged.
    // The resolved plan must carry exactly that ordinary rebuild, not an
    // additional control-view rebuild from its binding evidence.
    let final_control: Vec<_> = changes
        .iter()
        .filter(|step| {
            matches!(
                &step.change,
                Change::DropModule { id, .. }
                    | Change::CreateModule { id, .. }
                    | Change::AlterModule { id, .. }
                    if id == &control_id
            )
        })
        .collect();
    assert_eq!(final_control.len(), 2);
    assert!(matches!(
        &final_control[0].change,
        Change::DropModule { id, .. } if id == &control_id
    ));
    assert!(matches!(
        &final_control[1].change,
        Change::CreateModule { id, .. } if id == &control_id
    ));

    let table: pbps_model::TableName = format!("{}.t", cases::SCHEMA).parse().unwrap();
    let default = result
        .evidence
        .before()
        .prerequisites()
        .iter()
        .find(|record| {
            record.object.class == "pg_attrdef"
                && record.object.signature.first().is_some_and(|column| {
                    column.class == "column"
                        && column.name == ["c"]
                        && column
                            .signature
                            .first()
                            .is_some_and(|relation| relation.name == [cases::SCHEMA, "t"])
                })
        })
        .expect("the real target contains the declared ordinary default");
    assert_eq!(
        default.ownership,
        ObjectOwnership::Surface(Surface::Default(table.column("c"))),
        "a default is owned by the recorded default, not its column"
    );
    let unmanaged = result
        .evidence
        .before()
        .prerequisites()
        .iter()
        .find(|record| {
            record.object.class == "pg_class"
                && record.object.name == [cases::SCHEMA, "unmanaged_ix"]
        })
        .expect("unmanaged_ix must actually be captured before nonownership is claimed");
    assert_eq!(unmanaged.ownership, ObjectOwnership::Unqualified);
    assert!(
        result.changes.changes.iter().any(|change| {
            matches!(
                &change.change,
                Change::CreateModule { id, .. }
                    if id.to_string() == format!("{}.f(integer)", cases::SCHEMA)
            )
        }),
        "the final typed sequence creates the arriving overload"
    );
    let artifact = SavedPlan::new(
        PlanOrigin::Database,
        "postgres",
        "1274 native fixture",
        PlanBaseline {
            description: "independent target".into(),
            checksum: "00".repeat(32),
            database_collation: None,
        },
        result.changes,
        inputs.desired_ids.clone(),
    )
    .with_resolution(result.evidence)
    .unwrap();
    let restored: SavedPlan =
        serde_json::from_str(&serde_json::to_string(&artifact).unwrap()).unwrap();
    restored.validate_analysis().unwrap();
    let PlanAnalysis::Resolved(evidence) = &restored.analysis else {
        panic!("roundtripping producer evidence must retain resolved analysis");
    };
    pbps_pg::resolver::validate_evidence(evidence)
        .expect("roundtripped producer evidence must pass the artifact reader");
    assert_eq!(restored.checksum(), artifact.checksum());
}

async fn refusal(
    profile: Profile,
    statements: &[&str],
    expected: &str,
    index_parent: Option<&str>,
) {
    setup(statements).await;
    if let Some(parent) = index_parent {
        let mut peer = PeerVerifiedConn::connect(
            Driver::Postgres,
            &std::env::var("PBPS_NATIVE_CONNECTION").unwrap(),
        )
        .await
        .unwrap();
        let rows = peer
            .query(
                "SELECT tbl.relname::text AS parent FROM pg_catalog.pg_index i \
                 JOIN pg_catalog.pg_class idx ON idx.oid = i.indexrelid \
                 JOIN pg_catalog.pg_class tbl ON tbl.oid = i.indrelid \
                 JOIN pg_catalog.pg_namespace n ON n.oid = idx.relnamespace \
                 WHERE n.nspname = 'pbps_evidence1274' AND idx.relname = 'ix'",
            )
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].try_get::<&str>("parent").unwrap(), Some(parent));
    }
    let mut owned = matches!(profile, Profile::Container).then(ObservedContainers::begin);
    let mut target = target().await;
    let mut run = open(profile, &mut target).await;
    let inputs = Inputs::overload();
    let key = ProjectKey::new(true);
    let outcome = run
        .plan_resolved(
            &mut target,
            &inputs.binding(),
            inputs.base(),
            inputs.desired(),
            &inputs.hints,
            &[],
            &key.project,
            Some(ENVIRONMENT),
        )
        .await;
    let message = match outcome {
        Ok(_) => "the producer wrongly published a seal".to_owned(),
        Err(error) => error.to_string(),
    };
    close(&mut run, &mut owned).await;
    assert!(
        message.contains(expected),
        "expected the owned catalog mismatch ({expected}); got {message}"
    );
    // A refused fresh capture may consume its native target binding. Prove
    // the disposable engine remains usable through a new owned connection.
    drop(target);
    let mut fresh = self::target().await;
    fresh.check().await.unwrap();
    setup(&[]).await;
}

#[tokio::test]
#[ignore = "requires pinned native PostgreSQL target and owned Docker fixture"]
async fn the_container_producer_seals_the_overload_and_default_from_one_fresh_read() {
    positive(Profile::Container).await;
}

#[tokio::test]
#[ignore = "requires native supplied PostgreSQL and dedicated-server fixtures"]
async fn the_supplied_producer_seals_the_overload_and_default_from_one_fresh_read() {
    positive(Profile::Supplied).await;
}

/// The view's real catalog binding proves the undeclared relation is a read
/// prerequisite. Its system columns and index attributes remain unqualified;
/// only exact recorded table/index roots may own their physical children.
#[tokio::test]
#[ignore = "requires pinned native PostgreSQL target"]
async fn only_recorded_table_and_index_roots_own_their_catalog_columns() {
    use pbps_db::fingerprint::EnvironmentFingerprintKey;
    use pbps_model::resolver::ObjectIdentity;
    use pbps_pg::resolver::capture::{
        CandidateClass, CandidateSet, CaptureScope, RecordedOwnership,
        capture_identifying_qualified,
    };

    setup(cases::COLUMN_OWNERSHIP_TARGET_SETUP).await;
    let schema = cases::column_ownership_schema();
    let recorded_ids = ids(&schema, &IdsFile::default());
    let routines = BTreeMap::new();
    let dropped = BTreeMap::new();
    let namespaces = BTreeSet::from([cases::SCHEMA.to_owned()]);
    let recorded = RecordedOwnership {
        schema: &schema,
        ids: &recorded_ids,
        routines: &routines,
        dropped: &dropped,
        namespaces: &namespaces,
    };
    let scope = CaptureScope {
        retained: BTreeSet::new(),
        candidates: BTreeSet::from([CandidateSet {
            class: CandidateClass::Relation,
            namespace: Some(cases::SCHEMA.to_owned()),
            name: None,
        }]),
    };
    let key = ProjectKey::new(true);
    let selected = EnvironmentFingerprintKey::from_file(&key.root.join("key")).unwrap();
    let no_drops = BTreeSet::new();
    let mut peer = PeerVerifiedConn::connect(
        Driver::Postgres,
        &std::env::var("PBPS_NATIVE_CONNECTION").unwrap(),
    )
    .await
    .unwrap();
    let captured =
        capture_identifying_qualified(&mut peer, &scope, &no_drops, &selected, None, &recorded)
            .await;
    drop(peer);
    setup(&[]).await;
    let (_, manifest) = captured.expect("the fixed-key fresh catalog read must qualify");
    let relation = |name: &str| ObjectIdentity {
        class: "pg_class".into(),
        name: vec![cases::SCHEMA.into(), name.into()],
        signature: Vec::new(),
    };
    let attribute = |parent: &str, name: &str| ObjectIdentity {
        class: "column".into(),
        name: vec![name.into()],
        signature: vec![relation(parent)],
    };
    assert!(
        manifest.prerequisites().iter().any(|row| {
            row.object.class == "pg_rewrite"
                && row.object.signature.first() == Some(&relation("ref"))
                && row
                    .bindings
                    .iter()
                    .any(|binding| binding.target == relation("foreign_t"))
        }),
        "the managed view must actually read the undeclared table"
    );
    let table: pbps_model::TableName = format!("{}.t", cases::SCHEMA).parse().unwrap();
    for (name, expected) in [
        ("t", ObjectOwnership::Surface(Surface::Table(table.clone()))),
        (
            "ix",
            ObjectOwnership::Surface(Surface::Index {
                table: table.clone(),
                name: "ix".into(),
            }),
        ),
        ("unmanaged_ix", ObjectOwnership::Unqualified),
        ("foreign_t", ObjectOwnership::Unqualified),
        ("foreign_ix", ObjectOwnership::Unqualified),
    ] {
        let object = relation(name);
        let observed = manifest
            .prerequisites()
            .iter()
            .find(|row| row.object == object)
            .unwrap_or_else(|| panic!("the fresh capture omitted {object:?}"));
        assert_eq!(
            observed.ownership, expected,
            "recorded root authority for {object:?}"
        );
    }
    for (parent, name, expected) in [
        (
            "t",
            "cmax",
            ObjectOwnership::Surface(Surface::Table(table.clone())),
        ),
        (
            "t",
            "c",
            ObjectOwnership::Surface(Surface::Column(table.column("c"))),
        ),
        (
            "ix",
            "c",
            ObjectOwnership::Surface(Surface::Index {
                table: table.clone(),
                name: "ix".into(),
            }),
        ),
        ("unmanaged_ix", "c", ObjectOwnership::Unqualified),
        ("foreign_t", "cmax", ObjectOwnership::Unqualified),
        ("foreign_t", "c", ObjectOwnership::Unqualified),
        ("foreign_ix", "c", ObjectOwnership::Unqualified),
    ] {
        let object = attribute(parent, name);
        let observed = manifest
            .prerequisites()
            .iter()
            .find(|row| row.object == object)
            .unwrap_or_else(|| panic!("the fresh capture omitted {object:?}"));
        assert_eq!(
            observed.ownership, expected,
            "a shared column spelling or reference cannot confer ownership on {object:?}"
        );
    }
}

#[tokio::test]
#[ignore = "requires pinned native PostgreSQL target and owned Docker fixture"]
async fn a_same_named_view_cannot_own_the_declared_table_uid() {
    refusal(
        Profile::Container,
        cases::WRONG_KIND_TARGET_SETUP,
        "different catalog kind",
        None,
    )
    .await;
}

#[tokio::test]
#[ignore = "requires pinned native PostgreSQL target and owned Docker fixture"]
async fn a_same_named_index_on_another_table_cannot_own_the_declared_index() {
    refusal(
        Profile::Container,
        cases::WRONG_TABLE_INDEX_TARGET_SETUP,
        "different table",
        Some("other"),
    )
    .await;
}

#[tokio::test]
#[ignore = "requires pinned native PostgreSQL target and owned Docker fixture"]
async fn the_connected_producer_refuses_a_missing_environment_key_before_sealing() {
    setup(cases::TARGET_SETUP).await;
    let mut owned = Some(ObservedContainers::begin());
    let mut target = target().await;
    let mut run = open(Profile::Container, &mut target).await;
    let inputs = Inputs::overload();
    let key = ProjectKey::new(false);
    let outcome = run
        .plan_resolved(
            &mut target,
            &inputs.binding(),
            inputs.base(),
            inputs.desired(),
            &inputs.hints,
            &[],
            &key.project,
            Some(ENVIRONMENT),
        )
        .await;
    let message = match outcome {
        Ok(_) => "the producer wrongly published a keyless seal".to_owned(),
        Err(error) => error.to_string(),
    };
    close(&mut run, &mut owned).await;
    assert!(message.contains("pbps key generate"), "{message}");
    drop(target);
    let mut fresh = self::target().await;
    fresh.check().await.unwrap();
    setup(&[]).await;
}

fn catalog_scope(
    read: &pbps_model::resolver::ReadScope,
) -> pbps_pg::resolver::capture::CaptureScope {
    use pbps_pg::resolver::capture::{CandidateClass, CandidateSet, CaptureScope};
    let candidates = read
        .candidates
        .iter()
        .map(|set| CandidateSet {
            class: match set.class.as_str() {
                "pg_class" => CandidateClass::Relation,
                "pg_proc" => CandidateClass::Routine,
                "pg_type" => CandidateClass::Type,
                "pg_operator" => CandidateClass::Operator,
                "pg_collation" => CandidateClass::Collation,
                "pg_opclass" => CandidateClass::OperatorClass,
                "pg_opfamily" => CandidateClass::OperatorFamily,
                "pg_cast" => CandidateClass::Cast,
                "pg_extension" => CandidateClass::Extension,
                other => panic!("unrecognized qualified candidate class: {other}"),
            },
            namespace: set.namespace.clone(),
            name: set.name.clone(),
        })
        .collect();
    CaptureScope {
        retained: read.retained.clone(),
        candidates,
    }
}

// Apply uses Conn::execute, whose PostgreSQL driver sends the emitter's SQL
// as a batch. PeerVerifiedConn::query prepares one statement and cannot run
// the multi-command DDL emitted for some typed steps.
async fn execute_plan(conn: &mut pbps_db::Conn, changes: &ChangeSet) {
    execute_plan_with_extras(conn, changes, &[]).await;
}

async fn execute_plan_with_extras(
    conn: &mut pbps_db::Conn,
    changes: &ChangeSet,
    extras: &[String],
) {
    use pbps_dialect::Dialect;
    let dialect = pbps_pg::Postgres::with_write_path_extras(extras.to_vec());
    conn.execute("BEGIN").await.unwrap();
    for step in &changes.changes {
        for statement in dialect.emit(&step.change, step.strategy).unwrap() {
            conn.execute(&statement.sql).await.unwrap();
        }
    }
    conn.execute("COMMIT").await.unwrap();
}

/// PostgreSQL's typed AlterModule is expanded to DROP+CREATE, so a replaced
/// view receives creation defaults rather than retaining its old relation and
/// column ACL. Compare projected closing facts to a real post-DDL capture.
#[tokio::test]
#[ignore = "requires pinned native PostgreSQL target and owned Docker fixture"]
async fn a_rebuilt_view_projects_creation_defaults_instead_of_old_target_grants() {
    use pbps_db::fingerprint::EnvironmentFingerprintKey;
    use pbps_db::resolver::capture::ObjectIdentity;

    setup(cases::REBUILT_VIEW_TARGET_SETUP).await;
    let mut observer = PeerVerifiedConn::connect(
        Driver::Postgres,
        &std::env::var("PBPS_NATIVE_CONNECTION").unwrap(),
    )
    .await
    .unwrap();
    let original = observer
        .query(
            "SELECT c.relacl::text AS relation_acl, a.attacl::text AS column_acl \
             FROM pg_catalog.pg_class c \
             JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
             JOIN pg_catalog.pg_attribute a ON a.attrelid = c.oid AND a.attname = 'x' \
             WHERE n.nspname = 'pbps_evidence1274' AND c.relname = 'v'",
        )
        .await
        .unwrap();
    assert_eq!(original.len(), 1);
    assert!(
        original[0]
            .try_get::<&str>("relation_acl")
            .unwrap()
            .unwrap()
            .contains("pg_monitor")
    );
    assert!(
        original[0]
            .try_get::<&str>("column_acl")
            .unwrap()
            .unwrap()
            .contains("pg_monitor")
    );
    drop(observer);
    let mut owned = Some(ObservedContainers::begin());
    let mut target = target().await;
    let mut run = open(Profile::Container, &mut target).await;
    let inputs = Inputs::from_pair(cases::rebuilt_view_pair());
    let key = ProjectKey::new(true);
    let result = run
        .plan_resolved(
            &mut target,
            &inputs.binding(),
            inputs.base(),
            inputs.desired(),
            &inputs.hints,
            &[],
            &key.project,
            Some(ENVIRONMENT),
        )
        .await;
    let result = match result {
        Ok(result) => result,
        Err(error) => {
            close(&mut run, &mut owned).await;
            panic!("the view rebuild and its creation defaults must qualify: {error}");
        }
    };
    result.evidence.validate(&result.changes).unwrap();
    let view_id: pbps_model::ModuleId = "pbps_evidence1274.v".parse().unwrap();
    let dropped = result
        .changes
        .changes
        .iter()
        .position(|step| matches!(&step.change, Change::DropModule { id, .. } if id == &view_id))
        .expect("the final typed plan drops the old view");
    let created = result
        .changes
        .changes
        .iter()
        .position(|step| matches!(&step.change, Change::CreateModule { id, .. } if id == &view_id))
        .expect("the final typed plan creates the replacement view");
    assert!(dropped < created);

    let closing = result.evidence.after().clone();
    close(&mut run, &mut owned).await;
    let mut peer = pbps_db::Conn::connect(
        Driver::Postgres,
        &std::env::var("PBPS_NATIVE_CONNECTION").unwrap(),
    )
    .await
    .unwrap();
    execute_plan(&mut peer, &result.changes).await;
    let rows = peer
        .query("SELECT x FROM pbps_evidence1274.v")
        .await
        .unwrap();
    assert_eq!(rows[0].try_get::<i32>("x").unwrap(), Some(2));
    let acl = peer
        .query(
            "SELECT c.relacl::text AS relation_acl, a.attacl::text AS column_acl \
             FROM pg_catalog.pg_class c \
             JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
             JOIN pg_catalog.pg_attribute a ON a.attrelid = c.oid AND a.attname = 'x' \
             WHERE n.nspname = 'pbps_evidence1274' AND c.relname = 'v'",
        )
        .await
        .unwrap();
    assert_eq!(acl.len(), 1);
    assert_eq!(acl[0].try_get::<&str>("relation_acl").unwrap(), None);
    assert_eq!(acl[0].try_get::<&str>("column_acl").unwrap(), None);
    drop(peer);

    let selected = EnvironmentFingerprintKey::from_file(&key.root.join("key")).unwrap();
    let (_, actual) = target
        .capture_postgres_sealed(
            &catalog_scope(closing.scope()),
            &Default::default(),
            &selected,
        )
        .await
        .unwrap();
    let view = ObjectIdentity {
        class: "pg_class".into(),
        name: vec![cases::SCHEMA.into(), "v".into()],
        signature: Vec::new(),
    };
    let column = ObjectIdentity {
        class: "column".into(),
        name: vec!["x".into()],
        signature: vec![view.clone()],
    };
    let view_owner = ObjectOwnership::Surface(Surface::Module(view_id.clone()));
    for manifest in [result.evidence.before(), &closing] {
        for object in [&view, &column] {
            let record = manifest
                .prerequisites()
                .iter()
                .find(|record| &record.object == object)
                .expect("the rebuilt view and its column remain in the exact inventory");
            assert_eq!(
                record.ownership, view_owner,
                "the managed view owns its user column at both transition endpoints"
            );
        }
    }
    let encoded = serde_json::to_value(&result.evidence).unwrap();
    let transitions: Vec<pbps_model::resolver::ObjectTransition> =
        serde_json::from_value(encoded["transitions"].clone()).unwrap();
    let transition = transitions
        .iter()
        .find(|transition| transition.surface == Surface::Module(view_id.clone()))
        .expect("the typed rebuild has a view transition");
    assert!(
        transition.before.contains(&view)
            && transition.after.contains(&view)
            && transition.before.contains(&column)
            && transition.after.contains(&column),
        "the rebuilt view transition must replace both the relation and its user column"
    );
    for object in [view, column] {
        let expected = closing
            .prerequisites()
            .iter()
            .find(|record| record.object == object)
            .expect("projected closing evidence retains the target view and column");
        let observed = actual
            .prerequisites()
            .iter()
            .find(|record| record.object == expected.object)
            .expect("the fresh post-DDL target capture contains the same catalog object");
        assert_eq!(
            expected.properties, observed.properties,
            "the final projected {} properties must match actual rebuild defaults",
            object.class
        );
        assert_eq!(expected.bindings, observed.bindings);
    }
    target.check().await.unwrap();
    setup(&[]).await;
}

/// #614 first phase: a bare table must exist before f() can read it, while
/// its default, CHECK and index predicate must wait until f() exists. The
/// ordinary bootstrap is handed directly to the shared ScratchRun.
async fn empty_cross_kind_case(profile: Profile) {
    use pbps_db::fingerprint::EnvironmentFingerprintKey;

    setup(&["CREATE SCHEMA pbps_evidence1274"]).await;
    let mut owned = matches!(profile, Profile::Container).then(ObservedContainers::begin);
    let mut target = target().await;
    let mut run = open(profile, &mut target).await;
    let inputs = Inputs::from_pair(cases::empty_cross_kind_pair());
    let key = ProjectKey::new(true);
    let result = run
        .plan_resolved(
            &mut target,
            &inputs.binding(),
            inputs.base(),
            inputs.desired(),
            &inputs.hints,
            &[],
            &key.project,
            Some(ENVIRONMENT),
        )
        .await;
    let result = match result {
        Ok(result) => result,
        Err(error) => {
            close(&mut run, &mut owned).await;
            panic!("the empty target's ordinary bootstrap must reconstruct: {error}");
        }
    };
    result.evidence.validate(&result.changes).unwrap();
    let place = |predicate: fn(&Change) -> bool| {
        result
            .changes
            .changes
            .iter()
            .position(|step| predicate(&step.change))
            .expect("the final typed sequence includes every #614 creation phase")
    };
    let table = place(
        |change| matches!(change, Change::CreateTable { name, .. } if name.to_string() == "pbps_evidence1274.t"),
    );
    let routine = place(
        |change| matches!(change, Change::CreateModule { id, .. } if id.to_string() == "pbps_evidence1274.f()"),
    );
    let default = place(|change| matches!(change, Change::AlterColumnDefault { .. }));
    let check = place(|change| matches!(change, Change::AddCheck { .. }));
    let index = place(|change| matches!(change, Change::AddIndex { .. }));
    assert!(table < routine, "f() reads the new table");
    assert!(
        [default, check, index].into_iter().all(|at| routine < at),
        "all creation-time expressions call f()"
    );
    let actual_surfaces: std::collections::BTreeSet<_> = result
        .evidence
        .surfaces()
        .iter()
        .map(|row| row.surface.clone())
        .collect();
    let t: pbps_model::TableName = "pbps_evidence1274.t".parse().unwrap();
    for surface in [
        Surface::Default(t.column("id")),
        Surface::Check {
            table: t.clone(),
            name: "positive".into(),
        },
        Surface::Index {
            table: t,
            name: "ix".into(),
        },
        Surface::Module("pbps_evidence1274.f()".parse().unwrap()),
        Surface::Module("pbps_evidence1274.a()".parse().unwrap()),
    ] {
        assert!(
            actual_surfaces.contains(&surface),
            "the connected producer omits a #614 binding surface: {surface:?}"
        );
    }
    let closing = result.evidence.after().clone();
    close(&mut run, &mut owned).await;

    let mut peer = pbps_db::Conn::connect(
        Driver::Postgres,
        &std::env::var("PBPS_NATIVE_CONNECTION").unwrap(),
    )
    .await
    .unwrap();
    execute_plan(&mut peer, &result.changes).await;
    let selected = EnvironmentFingerprintKey::from_file(&key.root.join("key")).unwrap();
    let (_, observed) = target
        .capture_postgres_sealed(
            &catalog_scope(closing.scope()),
            &Default::default(),
            &selected,
        )
        .await
        .unwrap();
    for class in [
        "pg_class",
        "pg_proc",
        "pg_attrdef",
        "pg_constraint",
        "pg_index",
    ] {
        assert!(
            closing
                .prerequisites()
                .iter()
                .any(|record| record.object.class == class),
            "the projected closing manifest omits a #614 catalog class: {class}"
        );
    }
    for expected in closing.prerequisites() {
        let actual = observed
            .prerequisites()
            .iter()
            .find(|row| row.object == expected.object)
            .expect("every projected prerequisite exists after actual DDL");
        assert_eq!(
            expected.properties, actual.properties,
            "the projected catalog properties must match actual new-object defaults"
        );
        assert_eq!(expected.bindings, actual.bindings);
    }
    // The empty #614 phase proves ordered creation and the fresh closing
    // catalog. On PG16/18, after COMMIT the partial-index predicate recursively
    // invokes table-reading f() even for SELECT f() or an explicit INSERT.
    // The replacement phase below exercises DML after f() becomes constant.
    drop(peer);
    target.check().await.unwrap();
    setup(&[]).await;
}

#[tokio::test]
#[ignore = "requires pinned native PostgreSQL target and owned Docker fixture"]
async fn an_empty_target_producer_orders_table_routines_and_expressions_before_sealing() {
    empty_cross_kind_case(Profile::Container).await;
}

#[tokio::test]
#[ignore = "requires native supplied PostgreSQL and dedicated-server fixtures"]
async fn the_supplied_empty_target_producer_orders_table_routines_and_expressions_before_sealing() {
    empty_cross_kind_case(Profile::Supplied).await;
}

/// #614's same-name replacement must tear down the dependent expressions
/// before dropping f(), then recreate them after the new f() is present.
async fn replacement_case(profile: Profile) {
    setup(cases::REPLACEMENT_TARGET_SETUP).await;
    let mut owned = matches!(profile, Profile::Container).then(ObservedContainers::begin);
    let mut target = target().await;
    let mut run = open(profile, &mut target).await;
    let inputs = Inputs::from_pair(cases::replacement_pair());
    let key = ProjectKey::new(true);
    let result = run
        .plan_resolved(
            &mut target,
            &inputs.binding(),
            inputs.base(),
            inputs.desired(),
            &inputs.hints,
            &[],
            &key.project,
            Some(ENVIRONMENT),
        )
        .await;
    let result = match result {
        Ok(result) => result,
        Err(error) => {
            close(&mut run, &mut owned).await;
            panic!("the same-name routine replacement must be sealable: {error}");
        }
    };
    result.evidence.validate(&result.changes).unwrap();
    let changes = &result.changes.changes;
    let dropped_f = changes
        .iter()
        .position(|step| {
            matches!(&step.change, Change::DropModule { id, .. } if id.to_string() == "pbps_evidence1274.f()")
        })
        .expect("old f() must be dropped");
    assert!(
        changes[..dropped_f]
            .iter()
            .any(|step| matches!(step.change, Change::DropCheck { .. }))
            && changes[..dropped_f]
                .iter()
                .any(|step| matches!(step.change, Change::DropIndex { .. }))
            && changes[..dropped_f].iter().any(|step| {
                matches!(&step.change, Change::AlterColumnDefault { to: None, .. })
            })
            && changes[..dropped_f].iter().any(|step| {
                matches!(&step.change, Change::DropModule { id, .. } if id.to_string() == "pbps_evidence1274.a()")
            }),
        "CHECK, index, default and dependent routine must release old f() first"
    );
    let created_f = changes
        .iter()
        .position(|step| {
            matches!(&step.change, Change::CreateModule { id, .. } if id.to_string() == "pbps_evidence1274.f()")
        })
        .expect("new f() must be created");
    assert!(dropped_f < created_f);
    assert!(
        changes[created_f + 1..]
            .iter()
            .any(|step| { matches!(&step.change, Change::AlterColumnDefault { to: Some(_), .. }) }),
        "the default must be restored after new f()"
    );
    close(&mut run, &mut owned).await;
    let mut peer = pbps_db::Conn::connect(
        Driver::Postgres,
        &std::env::var("PBPS_NATIVE_CONNECTION").unwrap(),
    )
    .await
    .unwrap();
    execute_plan(&mut peer, &result.changes).await;
    peer.query("INSERT INTO pbps_evidence1274.t DEFAULT VALUES")
        .await
        .unwrap();
    let rows = peer
        .query("SELECT id, pbps_evidence1274.a() AS a FROM pbps_evidence1274.t")
        .await
        .unwrap();
    assert_eq!(rows[0].try_get::<i32>("id").unwrap(), Some(8));
    assert_eq!(rows[0].try_get::<i32>("a").unwrap(), Some(8));
    drop(peer);
    target.check().await.unwrap();
    setup(&[]).await;
}

#[tokio::test]
#[ignore = "requires pinned native PostgreSQL target and owned Docker fixture"]
async fn a_replaced_routine_rebuilds_cross_kind_dependents_in_the_final_plan() {
    replacement_case(Profile::Container).await;
}

#[tokio::test]
#[ignore = "requires native supplied PostgreSQL and dedicated-server fixtures"]
async fn the_supplied_replaced_routine_rebuilds_cross_kind_dependents_in_the_final_plan() {
    replacement_case(Profile::Supplied).await;
}

/// #614 records a table and column UID move before planning. The target still
/// holds the old spelling; a name-only reconstruction cannot authorize it.
async fn recorded_rename_case(profile: Profile, keep_public: bool, ordinary_table_owner: bool) {
    setup(cases::RENAME_TARGET_SETUP).await;
    if ordinary_table_owner {
        assert!(matches!(profile, Profile::Container));
        let mut peer = PeerVerifiedConn::connect(
            Driver::Postgres,
            &std::env::var("PBPS_NATIVE_CONNECTION").unwrap(),
        )
        .await
        .unwrap();
        peer.query("ALTER TABLE pbps_evidence1274.t OWNER TO pbps_native_alt")
            .await
            .unwrap();
        // The rebuilt predicate is checked as the table owner after PUBLIC is revoked.
        peer.query("GRANT EXECUTE ON ROUTINE pbps_evidence1274.f() TO pbps_native_alt")
            .await
            .unwrap();
        assert_eq!(
            catalog_owner_dependency("table-t").await,
            ("pbps_native_alt".into(), 1, 1),
            "the recorded table starts with its ordinary owner's exact edge"
        );
        assert_eq!(
            catalog_owner_dependency("index-ix").await,
            ("pbps_native_alt".into(), 0, 0),
            "the opening index inherits the table owner without its own owner edge"
        );
        let f_acl = routine_execution_acl("f", "pbps_native_alt").await;
        assert_eq!((f_acl.4, f_acl.5, f_acl.6), (1, 1, 0));
    }
    let opening_acl = table_column_grants("t", "id").await;
    assert_eq!(opening_acl.0 == "pbps_native_alt", ordinary_table_owner);
    assert!(
        routine_execution_acl("a", "pg_monitor").await.0,
        "a() begins with a NULL ACL"
    );
    let mut owned = matches!(profile, Profile::Container).then(ObservedContainers::begin);
    let mut target = target().await;
    let mut run = open(profile, &mut target).await;
    let (mut base, mut desired, mut base_ids, mut desired_ids) = cases::rename_pair();
    if ordinary_table_owner {
        let mut role = pbps_model::Role::default();
        role.grants.insert(
            "pbps_evidence1274.f()"
                .parse::<pbps_model::GrantTarget>()
                .unwrap(),
            BTreeSet::from([pbps_model::Permission::Execute]),
        );
        base.roles.insert("pbps_native_alt".into(), role.clone());
        desired.roles.insert("pbps_native_alt".into(), role);
        // Mint only the new role UID; keep the recorded table/column rename IDs.
        base_ids = ids(&base, &base_ids);
        let role_uid = base_ids.role_uid("pbps_native_alt").unwrap().clone();
        assert!(
            desired_ids
                .roles
                .insert(role_uid, "pbps_native_alt".into())
                .is_none()
        );
        assert_eq!(
            base_ids.role_uid("pbps_native_alt"),
            desired_ids.role_uid("pbps_native_alt")
        );
    }
    let old: pbps_model::TableName = "pbps_evidence1274.t".parse().unwrap();
    let new: pbps_model::TableName = "pbps_evidence1274.u".parse().unwrap();
    assert_eq!(base_ids.table_uid(&old), desired_ids.table_uid(&new));
    assert_eq!(
        base_ids.column_uid(&old.column("id")),
        desired_ids.column_uid(&new.column("n"))
    );
    let mut inputs = Inputs::with_ids(base, desired, base_ids, desired_ids);
    if keep_public {
        inputs
            .hints
            .public_execute
            .insert("pbps_evidence1274.a()".parse().unwrap());
    }
    let key = ProjectKey::new(true);
    let result = run
        .plan_resolved(
            &mut target,
            &inputs.binding(),
            inputs.base(),
            inputs.desired(),
            &inputs.hints,
            &[],
            &key.project,
            Some(ENVIRONMENT),
        )
        .await;
    let result = match result {
        Ok(result) => result,
        Err(error) => {
            close(&mut run, &mut owned).await;
            panic!("recorded table/column rename must be sealable: {error}");
        }
    };
    // The result owns its evidence and typed sequence. Release the runtime
    // before a negative oracle can strand an exact owned container.
    close(&mut run, &mut owned).await;
    result.evidence.validate(&result.changes).unwrap();
    let changes = &result.changes.changes;
    let table_rename = changes
        .iter()
        .position(|step| {
            matches!(&step.change, Change::RenameTable { uid, from, to, .. }
                if from == &old && to == &new && Some(uid) == inputs.base_ids.table_uid(&old))
        })
        .expect("the final plan carries the recorded table UID rename");
    let column_rename = changes
        .iter()
        .position(|step| {
            matches!(&step.change, Change::RenameColumn { uid, from, to, .. }
                if from == "id" && to == "n"
                    && Some(uid) == inputs.base_ids.column_uid(&old.column("id")))
        })
        .expect("the final plan carries the recorded column UID rename");
    let release = changes
        .iter()
        .position(|step| matches!(step.change, Change::DropCheck { .. }))
        .expect("the old expression must be released before rename");
    assert!(release < table_rename && table_rename < column_rename);
    let created_a = changes
        .iter()
        .position(|step| {
            matches!(&step.change, Change::CreateModule { id, .. }
                if id.to_string() == "pbps_evidence1274.a()")
        })
        .expect("the dependent a() is rebuilt");
    if ordinary_table_owner {
        let created_f = changes
            .iter()
            .position(|step| {
                matches!(&step.change, Change::CreateModule { id, .. }
                    if id.to_string() == "pbps_evidence1274.f()")
            })
            .expect("the predicate routine is rebuilt");
        let restored_f_grant = changes
            .iter()
            .position(|step| {
                matches!(&step.change, Change::Grant { role, target, permissions }
                    if role == "pbps_native_alt"
                        && target.to_string() == "pbps_evidence1274.f()"
                        && permissions == &BTreeSet::from([pbps_model::Permission::Execute]))
            })
            .expect("the final plan restores the table owner's routine EXECUTE");
        let restored_index = changes
            .iter()
            .position(|step| {
                matches!(&step.change, Change::AddIndex { table, name, .. }
                    if table == &new && name == "ix")
            })
            .expect("the dependent predicate index is restored");
        assert!(created_f < restored_f_grant && restored_f_grant < restored_index);
    }
    let public_a = changes
        .iter()
        .position(|step| {
            matches!(&step.change, Change::PublicExecution { routine, access, .. }
            if routine.to_string() == "pbps_evidence1274.a()"
                && *access == if keep_public {
                    pbps_model::PublicAccess::Kept
                } else {
                    pbps_model::PublicAccess::Revoked
                })
        })
        .expect("the plan explicitly settles PUBLIC execution of rebuilt a()");
    assert!(created_a < public_a, "the access decision follows CREATE");
    let closing = result.evidence.after().clone();
    let mut peer = pbps_db::Conn::connect(
        Driver::Postgres,
        &std::env::var("PBPS_NATIVE_CONNECTION").unwrap(),
    )
    .await
    .unwrap();
    execute_plan(&mut peer, &result.changes).await;
    drop(peer);
    if ordinary_table_owner {
        let f_acl = routine_execution_acl("f", "pbps_native_alt").await;
        assert_eq!(
            (f_acl.2, f_acl.4, f_acl.5, f_acl.6),
            (0, 1, 1, 0),
            "PUBLIC stays revoked while the table owner can execute the predicate"
        );
    }
    let acl = routine_execution_acl("a", "pg_monitor").await;
    assert!(!acl.0, "GRANT/REVOKE materializes a non-NULL routine ACL");
    assert_eq!(
        acl.1,
        Some(keep_public),
        "the final ACL reflects the opt-in"
    );
    assert_eq!(acl.2, i64::from(keep_public), "the PUBLIC entry is exact");
    assert_eq!(acl.3, 1, "the effective owner keeps EXECUTE");
    assert_eq!((acl.4, acl.5), (0, 0), "no role grant is invented");
    assert_eq!(
        catalog_owner_dependency("routine-a").await,
        ("postgres".into(), 0, 0),
        "the recreated routine's pinned owner has no shared dependency"
    );
    let routine_owner_edges: Vec<_> = closing
        .prerequisites()
        .iter()
        .filter(|row| {
            row.object.class == "pg_shdepend"
                && row.object.name == ["o"]
                && row.object.signature.first().is_some_and(|subject| {
                    subject.class == "pg_proc" && subject.name == [cases::SCHEMA, "a"]
                })
        })
        .collect();
    assert!(
        routine_owner_edges.is_empty(),
        "the sealed closing routine cannot invent a pinned owner edge: {routine_owner_edges:?}"
    );
    if ordinary_table_owner {
        assert_eq!(
            catalog_owner_dependency("table-u").await,
            ("pbps_native_alt".into(), 1, 1),
            "the in-place rename keeps the ordinary owner's exact edge"
        );
        let table_owner_edges: Vec<_> = closing
            .prerequisites()
            .iter()
            .filter(|row| {
                row.object.class == "pg_shdepend"
                    && row.object.name == ["o"]
                    && row.object.signature.first().is_some_and(|subject| {
                        subject.class == "pg_class" && subject.name == [cases::SCHEMA, "u"]
                    })
            })
            .collect();
        assert_eq!(table_owner_edges.len(), 1);
        let role = &table_owner_edges[0].object.signature[1];
        assert_eq!(role.class, "pg_authid");
        assert_eq!(role.name, ["pbps_native_alt"]);
        assert_eq!(
            catalog_owner_dependency("index-ix").await,
            ("pbps_native_alt".into(), 0, 0),
            "the rebuilt index inherits its ordinary table owner without an owner edge"
        );
        let index_owner_edges: Vec<_> = closing
            .prerequisites()
            .iter()
            .filter(|row| {
                row.object.class == "pg_shdepend"
                    && row.object.name == ["o"]
                    && row.object.signature.first().is_some_and(|subject| {
                        subject.class == "pg_class" && subject.name == [cases::SCHEMA, "ix"]
                    })
            })
            .collect();
        assert!(
            index_owner_edges.is_empty(),
            "the sealed closing index cannot invent an owner edge: {index_owner_edges:?}"
        );
    }
    let selected =
        pbps_db::fingerprint::EnvironmentFingerprintKey::from_file(&key.root.join("key")).unwrap();
    let (_, observed) = target
        .capture_postgres_sealed(
            &catalog_scope(closing.scope()),
            &Default::default(),
            &selected,
        )
        .await
        .unwrap();
    for expected in closing.prerequisites() {
        let actual = observed
            .prerequisites()
            .iter()
            .find(|row| row.object == expected.object)
            .unwrap_or_else(|| {
                let same_class: Vec<_> = observed
                    .prerequisites()
                    .iter()
                    .filter(|row| row.object.class == expected.object.class)
                    .map(|row| &row.object)
                    .collect();
                panic!(
                    "the UID-projected object exists under its new catalog identity: \
                     missing {:?}; observed same-class identities: {:?}",
                    expected.object, same_class
                );
            });
        assert_eq!(
            expected.properties, actual.properties,
            "projected closing properties differ for {:?}",
            expected.object
        );
        assert_eq!(
            expected.bindings, actual.bindings,
            "projected closing bindings differ for {:?}",
            expected.object
        );
    }
    assert_eq!(table_column_grants("u", "n").await, opening_acl);
    let mut peer = PeerVerifiedConn::connect(
        Driver::Postgres,
        &std::env::var("PBPS_NATIVE_CONNECTION").unwrap(),
    )
    .await
    .unwrap();
    peer.query("INSERT INTO pbps_evidence1274.u DEFAULT VALUES")
        .await
        .unwrap();
    let rows = peer
        .query("SELECT n, pbps_evidence1274.a() AS a FROM pbps_evidence1274.u")
        .await
        .unwrap();
    assert_eq!(rows[0].try_get::<i32>("n").unwrap(), Some(9));
    assert_eq!(rows[0].try_get::<i32>("a").unwrap(), Some(9));
    drop(peer);
    target.check().await.unwrap();
    setup(&[]).await;
}

#[tokio::test]
#[ignore = "requires pinned native PostgreSQL target and owned Docker fixture"]
async fn recorded_table_and_column_uids_survive_rename_with_dependent_rebuilds() {
    recorded_rename_case(Profile::Container, false, false).await;
}

#[tokio::test]
#[ignore = "requires native supplied PostgreSQL and dedicated-server fixtures"]
async fn the_supplied_recorded_table_and_column_uids_survive_rename_with_dependent_rebuilds() {
    recorded_rename_case(Profile::Supplied, false, false).await;
}

#[tokio::test]
#[ignore = "requires pinned native PostgreSQL target and owned Docker fixture"]
async fn recorded_rename_retains_the_ordinary_table_owner_dependency() {
    recorded_rename_case(Profile::Container, false, true).await;
}

#[tokio::test]
#[ignore = "requires pinned native PostgreSQL target and owned Docker fixture"]
async fn rebuilt_routine_with_explicit_public_execution_matches_the_post_ddl_acl() {
    recorded_rename_case(Profile::Container, true, false).await;
}

#[tokio::test]
#[ignore = "requires native supplied PostgreSQL and dedicated-server fixtures"]
async fn the_supplied_rebuilt_routine_with_explicit_public_execution_matches_the_post_ddl_acl() {
    recorded_rename_case(Profile::Supplied, true, false).await;
}

/// A binding-induced routine rebuild must replay only the declared role
/// grants. The fixture carries both an ordinary role (with an ACL shared
/// dependency) and the pinned pg_monitor role (without that dependency).
async fn rebuilt_routine_role_acl_case(grant_option: bool) {
    use pbps_db::fingerprint::EnvironmentFingerprintKey;
    use pbps_model::{GrantTarget, Permission, Role};

    let mut statements = cases::REPLACEMENT_TARGET_SETUP.to_vec();
    statements.push(if grant_option {
        "GRANT EXECUTE ON ROUTINE pbps_evidence1274.a() TO pbps_native_alt WITH GRANT OPTION"
    } else {
        "GRANT EXECUTE ON ROUTINE pbps_evidence1274.a() TO pbps_native_alt"
    });
    statements.push("GRANT EXECUTE ON ROUTINE pbps_evidence1274.a() TO pg_monitor");
    setup(&statements).await;
    let mut peer = PeerVerifiedConn::connect(
        Driver::Postgres,
        &std::env::var("PBPS_NATIVE_CONNECTION").unwrap(),
    )
    .await
    .unwrap();
    let roles = peer
        .query(
            "SELECT rolname, oid::int8 AS oid FROM pg_catalog.pg_roles \
              WHERE rolname IN ('pbps_native_alt', 'pg_monitor') ORDER BY rolname",
        )
        .await
        .unwrap();
    assert_eq!(
        roles.len(),
        2,
        "the native fixture supplies both role classes"
    );
    for row in &roles {
        let name = row.try_get::<&str>("rolname").unwrap().unwrap();
        let oid = row.try_get::<i64>("oid").unwrap().unwrap();
        assert_eq!(oid >= 12000, name == "pbps_native_alt");
    }
    drop(peer);
    let mut owned = Some(ObservedContainers::begin());
    let mut target = target().await;
    let mut run = open(Profile::Container, &mut target).await;
    let (mut base, mut desired) = cases::replacement_pair();
    for name in ["pbps_native_alt", "pg_monitor"] {
        let mut role = Role::default();
        role.grants.insert(
            "pbps_evidence1274.a()".parse::<GrantTarget>().unwrap(),
            BTreeSet::from([Permission::Execute]),
        );
        base.roles.insert(name.into(), role.clone());
        desired.roles.insert(name.into(), role);
    }
    let inputs = Inputs::from_pair((base, desired));
    let key = ProjectKey::new(true);
    let result = run
        .plan_resolved(
            &mut target,
            &inputs.binding(),
            inputs.base(),
            inputs.desired(),
            &inputs.hints,
            &[],
            &key.project,
            Some(ENVIRONMENT),
        )
        .await;
    close(&mut run, &mut owned).await;
    if grant_option {
        let error = match result {
            Err(error) => error,
            Ok(_) => panic!("a grant option absent from declarations must refuse sealing"),
        };
        assert!(
            error
                .to_string()
                .to_ascii_lowercase()
                .contains("grant option"),
            "the refusal identifies the unrepresentable option: {error}"
        );
        target.check().await.unwrap();
        setup(&[]).await;
        return;
    }
    let result = result.expect("both declared routine grants can be restored");
    result.evidence.validate(&result.changes).unwrap();
    let created = result
        .changes
        .changes
        .iter()
        .position(|step| {
            matches!(&step.change, Change::CreateModule { id, .. }
            if id.to_string() == "pbps_evidence1274.a()")
        })
        .expect("the dependent routine is recreated");
    for name in ["pbps_native_alt", "pg_monitor"] {
        let grant = result
            .changes
            .changes
            .iter()
            .position(|step| {
                matches!(&step.change, Change::Grant { role, target, permissions }
                if role == name && target.to_string() == "pbps_evidence1274.a()"
                    && permissions == &BTreeSet::from([Permission::Execute]))
            })
            .expect("the final plan restores the declared role grant");
        assert!(created < grant, "the grant follows routine recreation");
    }
    let closing = result.evidence.after().clone();
    let mut peer = pbps_db::Conn::connect(
        Driver::Postgres,
        &std::env::var("PBPS_NATIVE_CONNECTION").unwrap(),
    )
    .await
    .unwrap();
    execute_plan(&mut peer, &result.changes).await;
    drop(peer);
    let ordinary = routine_execution_acl("a", "pbps_native_alt").await;
    let pinned = routine_execution_acl("a", "pg_monitor").await;
    assert_eq!(
        (
            ordinary.0, ordinary.2, ordinary.3, ordinary.4, ordinary.5, ordinary.6
        ),
        (false, 0, 1, 1, 1, 0)
    );
    assert_eq!(
        (pinned.4, pinned.5, pinned.6),
        (1, 0, 0),
        "pinned grants have no ACL role edge"
    );
    let selected = EnvironmentFingerprintKey::from_file(&key.root.join("key")).unwrap();
    let (_, observed) = target
        .capture_postgres_sealed(
            &catalog_scope(closing.scope()),
            &Default::default(),
            &selected,
        )
        .await
        .unwrap();
    let object = closing
        .prerequisites()
        .iter()
        .find(|record| {
            record.object.class == "pg_proc" && record.object.name == [cases::SCHEMA, "a"]
        })
        .expect("the projected routine is present");
    let actual = observed
        .prerequisites()
        .iter()
        .find(|record| record.object == object.object)
        .expect("the same routine exists after DDL");
    assert_eq!(
        object.properties, actual.properties,
        "final role grants affect proacl"
    );
    assert_eq!(object.bindings, actual.bindings);
    // The SQL ACL oracle above cannot prove that the projected dependency
    // inventory retained the ordinary role's distinct shared edge.
    let acl_edge = |role: &str| pbps_db::resolver::capture::ObjectIdentity {
        class: "pg_shdepend".into(),
        name: vec!["a".into()],
        signature: vec![
            object.object.clone(),
            pbps_db::resolver::capture::ObjectIdentity {
                class: "pg_authid".into(),
                name: vec![role.into()],
                signature: Vec::new(),
            },
        ],
    };
    let ordinary_edge = acl_edge("pbps_native_alt");
    let projected: Vec<_> = closing
        .prerequisites()
        .iter()
        .filter(|row| row.object == ordinary_edge)
        .collect();
    let captured: Vec<_> = observed
        .prerequisites()
        .iter()
        .filter(|row| row.object == ordinary_edge)
        .collect();
    assert_eq!(
        projected.len(),
        1,
        "the projected ordinary ACL edge is exact"
    );
    assert_eq!(
        captured.len(),
        1,
        "the fresh target has the ordinary ACL edge"
    );
    assert_eq!(projected[0].properties, captured[0].properties);
    assert_eq!(projected[0].bindings, captured[0].bindings);
    let pinned_edge = acl_edge("pg_monitor");
    assert!(
        closing
            .prerequisites()
            .iter()
            .all(|row| row.object != pinned_edge)
            && observed
                .prerequisites()
                .iter()
                .all(|row| row.object != pinned_edge),
        "a pinned role's grant has no shared ACL edge in either inventory"
    );
    target.check().await.unwrap();
    setup(&[]).await;
}

#[tokio::test]
#[ignore = "requires pinned native PostgreSQL target and owned Docker fixture"]
async fn rebuilt_routine_replays_declared_grants_and_only_ordinary_role_acl_edges() {
    rebuilt_routine_role_acl_case(false).await;
}

#[tokio::test]
#[ignore = "requires pinned native PostgreSQL target and owned Docker fixture"]
async fn unrepresentable_routine_grant_option_refuses_before_evidence() {
    rebuilt_routine_role_acl_case(true).await;
}

/// An unrelated column arrival cannot turn an unchanged routine's opening
/// grant option into a rebuild refusal or silently flatten that ACL in evidence.
#[tokio::test]
#[ignore = "requires pinned native PostgreSQL target and owned Docker fixture"]
async fn an_unaffected_routine_keeps_its_grant_option_through_connected_evidence() {
    use pbps_db::fingerprint::EnvironmentFingerprintKey;
    use pbps_model::{GrantTarget, Permission, Role};

    let mut statements = cases::REPLACEMENT_TARGET_SETUP.to_vec();
    statements.push(
        "GRANT EXECUTE ON ROUTINE pbps_evidence1274.a() TO pbps_native_alt WITH GRANT OPTION",
    );
    setup(&statements).await;
    let opening_acl = routine_execution_acl("a", "pbps_native_alt").await;
    assert_eq!((opening_acl.5, opening_acl.6), (1, 1));

    let mut owned = Some(ObservedContainers::begin());
    let mut target = target().await;
    let mut run = open(Profile::Container, &mut target).await;
    let (_, mut base) = cases::empty_cross_kind_pair();
    let mut desired = base.clone();
    desired
        .tables
        .get_mut(&"pbps_evidence1274.t".parse().unwrap())
        .unwrap()
        .columns
        .insert(
            "note".into(),
            pbps_model::Column::new("integer".parse().unwrap()),
        );
    let mut role = Role::default();
    role.grants.insert(
        "pbps_evidence1274.a()".parse::<GrantTarget>().unwrap(),
        BTreeSet::from([Permission::Execute]),
    );
    base.roles.insert("pbps_native_alt".into(), role.clone());
    desired.roles.insert("pbps_native_alt".into(), role);
    let inputs = Inputs::from_pair((base, desired));
    let key = ProjectKey::new(true);
    let result = run
        .plan_resolved(
            &mut target,
            &inputs.binding(),
            inputs.base(),
            inputs.desired(),
            &inputs.hints,
            &[],
            &key.project,
            Some(ENVIRONMENT),
        )
        .await;
    close(&mut run, &mut owned).await;
    let result = result.expect("an untouched routine does not require ACL restoration");
    result.evidence.validate(&result.changes).unwrap();
    assert!(result.changes.changes.iter().any(|step| {
        matches!(&step.change, Change::AddColumn { table, name, .. }
            if table.to_string() == "pbps_evidence1274.t" && name == "note")
    }));
    assert!(!result.changes.changes.iter().any(|step| {
        matches!(&step.change,
            Change::DropModule { id, .. }
            | Change::AlterModule { id, .. }
            | Change::CreateModule { id, .. }
            if id.to_string() == "pbps_evidence1274.a()")
    }));
    let before = result
        .evidence
        .before()
        .prerequisites()
        .iter()
        .find(|row| row.object.class == "pg_proc" && row.object.name == [cases::SCHEMA, "a"])
        .expect("the opening routine is captured");
    let closing = result.evidence.after().clone();
    let after = closing
        .prerequisites()
        .iter()
        .find(|row| row.object == before.object)
        .expect("the unchanged routine remains in the closing inventory");
    assert_eq!(before.properties, after.properties);
    assert_eq!(before.bindings, after.bindings);

    let mut peer = pbps_db::Conn::connect(
        Driver::Postgres,
        &std::env::var("PBPS_NATIVE_CONNECTION").unwrap(),
    )
    .await
    .unwrap();
    execute_plan(&mut peer, &result.changes).await;
    drop(peer);
    assert_eq!(
        routine_execution_acl("a", "pbps_native_alt").await,
        opening_acl
    );
    let selected = EnvironmentFingerprintKey::from_file(&key.root.join("key")).unwrap();
    let (_, observed) = target
        .capture_postgres_sealed(
            &catalog_scope(closing.scope()),
            &Default::default(),
            &selected,
        )
        .await
        .unwrap();
    let actual = observed
        .prerequisites()
        .iter()
        .find(|row| row.object == after.object)
        .expect("the actual post-DDL routine remains in scope");
    assert_eq!(after.properties, actual.properties);
    assert_eq!(after.bindings, actual.bindings);
    target.check().await.unwrap();
    setup(&[]).await;
}

/// The new routines are created under the target creator's schema defaults,
/// not the scratch role's defaults. PUBLIC is then revoked by the ordered plan;
/// the ordinary role's option and shared dependency must survive that revoke.
#[tokio::test]
#[ignore = "requires pinned native PostgreSQL target and owned Docker fixture"]
async fn newly_created_routines_project_schema_default_grants_then_public_revoke() {
    use pbps_db::fingerprint::EnvironmentFingerprintKey;

    setup(&[
        "CREATE SCHEMA pbps_evidence1274",
        "ALTER DEFAULT PRIVILEGES IN SCHEMA pbps_evidence1274 GRANT EXECUTE ON FUNCTIONS TO pbps_native_alt WITH GRANT OPTION",
        "ALTER DEFAULT PRIVILEGES IN SCHEMA pbps_evidence1274 GRANT EXECUTE ON FUNCTIONS TO pg_monitor",
    ])
    .await;
    let mut owned = Some(ObservedContainers::begin());
    let mut target = target().await;
    let mut run = open(Profile::Container, &mut target).await;
    let inputs = Inputs::from_pair(cases::empty_cross_kind_pair());
    let key = ProjectKey::new(true);
    let result = run
        .plan_resolved(
            &mut target,
            &inputs.binding(),
            inputs.base(),
            inputs.desired(),
            &inputs.hints,
            &[],
            &key.project,
            Some(ENVIRONMENT),
        )
        .await;
    let result = match result {
        Ok(result) => result,
        Err(error) => {
            close(&mut run, &mut owned).await;
            panic!("the target's captured creation defaults must qualify: {error}");
        }
    };
    close(&mut run, &mut owned).await;
    result.evidence.validate(&result.changes).unwrap();
    for name in ["f", "a"] {
        let create = result
            .changes
            .changes
            .iter()
            .position(|step| {
                matches!(&step.change, Change::CreateModule { id, .. }
                if id.to_string() == format!("pbps_evidence1274.{name}()"))
            })
            .expect("the ordinary bootstrap creates the routine");
        let revoke = result
            .changes
            .changes
            .iter()
            .position(|step| {
                matches!(&step.change, Change::PublicExecution { routine, access, .. }
                if routine.to_string() == format!("pbps_evidence1274.{name}()")
                    && *access == pbps_model::PublicAccess::Revoked)
            })
            .expect("the final plan closes the created routine to PUBLIC");
        assert!(create < revoke);
    }
    let closing = result.evidence.after().clone();
    let mut peer = pbps_db::Conn::connect(
        Driver::Postgres,
        &std::env::var("PBPS_NATIVE_CONNECTION").unwrap(),
    )
    .await
    .unwrap();
    execute_plan(&mut peer, &result.changes).await;
    drop(peer);
    for name in ["f", "a"] {
        let ordinary = routine_execution_acl(name, "pbps_native_alt").await;
        let pinned = routine_execution_acl(name, "pg_monitor").await;
        assert_eq!(
            (
                ordinary.0, ordinary.2, ordinary.3, ordinary.4, ordinary.5, ordinary.6
            ),
            (false, 0, 1, 0, 1, 1),
            "PUBLIC revoke retains the ordinary role's grant option and edge"
        );
        assert_eq!((pinned.4, pinned.5, pinned.6), (1, 0, 0));
    }
    let selected = EnvironmentFingerprintKey::from_file(&key.root.join("key")).unwrap();
    let (_, observed) = target
        .capture_postgres_sealed(
            &catalog_scope(closing.scope()),
            &Default::default(),
            &selected,
        )
        .await
        .unwrap();
    for name in ["f", "a"] {
        let expected = closing
            .prerequisites()
            .iter()
            .find(|row| row.object.class == "pg_proc" && row.object.name == [cases::SCHEMA, name])
            .expect("the final projection includes the new routine");
        let actual = observed
            .prerequisites()
            .iter()
            .find(|row| row.object == expected.object)
            .expect("the post-DDL target contains the same routine");
        assert_eq!(
            expected.properties, actual.properties,
            "creation defaults and final grants are both projected"
        );
        assert_eq!(expected.bindings, actual.bindings);
    }
    target.check().await.unwrap();
    setup(&[]).await;
}

/// A recorded rename frees a spelling another UID may take. A check dropped
/// from the old table belongs to its opening UID exactly once.
#[test]
fn reused_table_spelling_does_not_duplicate_an_opening_child_inventory() {
    use pbps_db::resolver::capture::ObjectIdentity;
    use pbps_model::{CheckConstraint, Column, PlannedChange, Table};
    use pbps_pg::resolver::capture::BindingRecord;
    use std::collections::BTreeSet;

    let old: pbps_model::TableName = "app.a".parse().unwrap();
    let renamed: pbps_model::TableName = "app.b".parse().unwrap();
    let other: pbps_model::TableName = "app.c".parse().unwrap();
    let mut first = Table::default();
    first
        .columns
        .insert("id".into(), Column::new("integer".parse().unwrap()));
    first.checks.insert(
        "ck".into(),
        CheckConstraint {
            expression: "id > 0".into(),
        },
    );
    let mut second = Table::default();
    second
        .columns
        .insert("id".into(), Column::new("integer".parse().unwrap()));
    let mut base = Schema::default();
    base.tables.insert(old.clone(), first.clone());
    base.tables.insert(other.clone(), second.clone());
    let mut desired = Schema::default();
    first.checks.clear();
    desired.tables.insert(renamed.clone(), first);
    desired.tables.insert(old.clone(), second);
    let base_ids = ids(&base, &IdsFile::default());
    let first_uid = base_ids.table_uid(&old).unwrap().clone();
    let second_uid = base_ids.table_uid(&other).unwrap().clone();
    let mut desired_ids = base_ids.clone();
    desired_ids.rename_table(&old, &renamed);
    desired_ids.rename_table(&other, &old);
    assert_eq!(desired_ids.table_uid(&renamed), Some(&first_uid));
    assert_eq!(desired_ids.table_uid(&old), Some(&second_uid));
    assert_ne!(first_uid, second_uid);

    let relation = |name: &pbps_model::TableName| ObjectIdentity {
        class: "pg_class".into(),
        name: vec![name.schema.clone(), name.name.clone()],
        signature: vec![],
    };
    let old_relation = relation(&old);
    let other_relation = relation(&other);
    let renamed_relation = relation(&renamed);
    let reused_relation = relation(&old);
    let check = ObjectIdentity {
        class: "pg_constraint".into(),
        name: vec![old.schema.clone(), "ck".into()],
        signature: vec![old_relation.clone()],
    };
    let check_surface = Surface::Check {
        table: old.clone(),
        name: "ck".into(),
    };
    let owned = |object, surface| BindingRecord {
        object,
        ownership: ObjectOwnership::Surface(surface),
        bindings: vec![],
    };
    let opening = vec![
        owned(old_relation.clone(), Surface::Table(old.clone())),
        owned(other_relation.clone(), Surface::Table(other.clone())),
        owned(check.clone(), check_surface.clone()),
    ];
    let compiled = vec![
        owned(renamed_relation.clone(), Surface::Table(renamed.clone())),
        owned(reused_relation.clone(), Surface::Table(old.clone())),
    ];
    let changes = ChangeSet {
        changes: vec![
            PlannedChange::new(Change::DropCheck {
                table: old.clone(),
                name: "ck".into(),
            }),
            PlannedChange::new(Change::RenameTable {
                uid: first_uid,
                from: old.clone(),
                to: renamed.clone(),
                defaults: vec![],
            }),
            PlannedChange::new(Change::RenameTable {
                uid: second_uid,
                from: other,
                to: old.clone(),
                defaults: vec![],
            }),
        ],
    };
    let transitions = super::transitions::derive(
        &changes,
        pbps_diff::Side {
            schema: &base,
            ids: &base_ids,
        },
        pbps_diff::Side {
            schema: &desired,
            ids: &desired_ids,
        },
        &opening,
        &compiled,
    )
    .unwrap();
    let first_table = transitions
        .iter()
        .find(|t| t.surface == Surface::Table(renamed.clone()))
        .unwrap();
    let reused_table = transitions
        .iter()
        .find(|t| t.surface == Surface::Table(old.clone()))
        .unwrap();
    let dropped_check = transitions
        .iter()
        .find(|t| t.surface == check_surface)
        .unwrap();
    assert_eq!(dropped_check.before, BTreeSet::from([check]));
    assert_eq!(first_table.before, BTreeSet::from([old_relation]));
    assert_eq!(reused_table.before, BTreeSet::from([other_relation]));
    assert_eq!(first_table.after, BTreeSet::from([renamed_relation]));
    assert_eq!(reused_table.after, BTreeSet::from([reused_relation]));
    let claimed: Vec<_> = transitions
        .iter()
        .flat_map(|t| t.before.iter().cloned())
        .collect();
    assert_eq!(claimed.len(), claimed.iter().collect::<BTreeSet<_>>().len());
}

/// An ordinary default removal after recorded table/column renames names the
/// final column. Its opening default belongs to the old column UID, and an
/// unrecorded UID must refuse instead of claiming an empty inventory.
#[test]
fn final_coordinate_default_removal_uses_the_recorded_opening_uid() {
    use pbps_db::resolver::capture::ObjectIdentity;
    use pbps_model::{Column, PlannedChange, Table};
    use pbps_pg::resolver::capture::BindingRecord;
    use std::collections::BTreeSet;

    let old: pbps_model::TableName = "app.a".parse().unwrap();
    let renamed: pbps_model::TableName = "app.b".parse().unwrap();
    let mut table = Table::default();
    let mut column = Column::new("integer".parse().unwrap());
    column.default = Some("1".into());
    table.columns.insert("id".into(), column);
    let mut base = Schema::default();
    base.tables.insert(old.clone(), table.clone());
    let mut desired = Schema::default();
    let mut final_column = table.columns.shift_remove("id").unwrap();
    final_column.default = None;
    table.columns.insert("n".into(), final_column);
    desired.tables.insert(renamed.clone(), table);
    let base_ids = ids(&base, &IdsFile::default());
    let table_uid = base_ids.table_uid(&old).unwrap().clone();
    let column_uid = base_ids.column_uid(&old.column("id")).unwrap().clone();
    let mut desired_ids = base_ids.clone();
    desired_ids.rename_table(&old, &renamed);
    desired_ids.columns.get_mut(&column_uid).unwrap().name = "n".into();
    assert_eq!(
        desired_ids.column_uid(&renamed.column("n")),
        Some(&column_uid)
    );

    let relation = ObjectIdentity {
        class: "pg_class".into(),
        name: vec![old.schema.clone(), old.name.clone()],
        signature: vec![],
    };
    let old_column = ObjectIdentity {
        class: "column".into(),
        name: vec!["id".into()],
        signature: vec![relation],
    };
    let default = ObjectIdentity {
        class: "pg_attrdef".into(),
        name: vec!["id_default".into()],
        signature: vec![old_column],
    };
    let opening = vec![BindingRecord {
        object: default.clone(),
        ownership: ObjectOwnership::Surface(Surface::Default(old.column("id"))),
        bindings: vec![],
    }];
    let changes = ChangeSet {
        changes: vec![
            PlannedChange::new(Change::RenameTable {
                uid: table_uid.clone(),
                from: old.clone(),
                to: renamed.clone(),
                defaults: vec!["id".into()],
            }),
            PlannedChange::new(Change::RenameColumn {
                uid: column_uid.clone(),
                table: renamed.clone(),
                from: "id".into(),
                to: "n".into(),
                table_was: Some(old.clone()),
            }),
            PlannedChange::new(Change::AlterColumnDefault {
                uid: column_uid,
                column: renamed.column("n"),
                from: Some("1".into()),
                to: None,
            }),
        ],
    };
    let derive = |plan: &ChangeSet| {
        super::transitions::derive(
            plan,
            pbps_diff::Side {
                schema: &base,
                ids: &base_ids,
            },
            pbps_diff::Side {
                schema: &desired,
                ids: &desired_ids,
            },
            &opening,
            &[],
        )
    };
    let transitions = derive(&changes).unwrap();
    let removal = transitions
        .iter()
        .find(|t| t.surface == Surface::Default(renamed.column("n")))
        .unwrap();
    assert_eq!(removal.before, BTreeSet::from([default]));
    assert!(removal.after.is_empty());
    let mut wrong_uid = changes.clone();
    let Change::AlterColumnDefault { uid, .. } = &mut wrong_uid.changes[2].change else {
        panic!("the final typed step removes the default");
    };
    *uid = table_uid;
    assert!(derive(&wrong_uid).is_err());
}

// These qualified-record fixtures exercise the producer's actual resolution
// and transition gates. Raw OIDs are not logical addresses: PG18 replaced the
// measured generated attrdef while preserving its table and column.
fn generation_schema(table: &pbps_model::TableName) -> Schema {
    use pbps_model::{Column, Generated, Table};

    let mut definition = Table::default();
    definition
        .columns
        .insert("a".into(), Column::new("integer".parse().unwrap()));
    let mut generated = Column::new("integer".parse().unwrap());
    generated.generated = Some(Generated {
        expression: "a * 2 + 1".into(),
        stored: true,
    });
    definition.columns.insert("g".into(), generated);
    let mut ordinary = Column::new("integer".parse().unwrap());
    ordinary.default = Some("7".into());
    definition.columns.insert("d".into(), ordinary);
    let mut schema = Schema::default();
    schema.tables.insert(table.clone(), definition);
    schema
}

fn generation_objects(
    table: &pbps_model::TableName,
    column: &str,
) -> [pbps_model::resolver::ObjectIdentity; 4] {
    use pbps_model::resolver::ObjectIdentity;

    let relation = ObjectIdentity {
        class: "pg_class".into(),
        name: vec![table.schema.clone(), table.name.clone()],
        signature: vec![],
    };
    let column = ObjectIdentity {
        class: "column".into(),
        name: vec![column.into()],
        signature: vec![relation.clone()],
    };
    let attrdef = ObjectIdentity {
        class: "pg_attrdef".into(),
        name: vec![],
        signature: vec![column.clone()],
    };
    let owner = ObjectIdentity {
        class: "pg_depend".into(),
        name: vec!["i".into()],
        signature: vec![attrdef.clone(), column.clone()],
    };
    let reference = ObjectIdentity {
        class: "pg_depend".into(),
        name: vec!["n".into()],
        signature: vec![
            attrdef.clone(),
            ObjectIdentity {
                class: "column".into(),
                name: vec!["a".into()],
                signature: vec![relation],
            },
        ],
    };
    [column, attrdef, owner, reference]
}

fn generation_records(
    table: &pbps_model::TableName,
    generated_name: &str,
) -> Vec<pbps_pg::resolver::capture::BindingRecord> {
    use pbps_pg::resolver::capture::BindingRecord;

    let owned = |object, surface| BindingRecord {
        object,
        ownership: ObjectOwnership::Surface(surface),
        bindings: vec![],
    };
    let [generated, attrdef, owner, reference] = generation_objects(table, generated_name);
    let relation = generated.signature[0].clone();
    let [source, ..] = generation_objects(table, "a");
    let [ordinary, default, mut automatic, ..] = generation_objects(table, "d");
    automatic.name = vec!["a".into()];
    let surface = Surface::Default(table.column(generated_name));
    vec![
        owned(relation, Surface::Table(table.clone())),
        owned(source, Surface::Column(table.column("a"))),
        owned(generated, Surface::Column(table.column(generated_name))),
        owned(ordinary, Surface::Column(table.column("d"))),
        owned(attrdef, surface.clone()),
        owned(owner, surface.clone()),
        owned(reference, surface),
        owned(default, Surface::Default(table.column("d"))),
        owned(automatic, Surface::Default(table.column("d"))),
    ]
}

#[test]
fn generated_expression_transitions_keep_exact_inventory_and_recorded_rename_identity() {
    use pbps_model::PlannedChange;

    let old = pbps_model::TableName::new("app", "t");
    for rename in [false, true] {
        let final_table = if rename {
            pbps_model::TableName::new("app", "renamed")
        } else {
            old.clone()
        };
        let final_name = if rename { "n" } else { "g" };
        let base = generation_schema(&old);
        let base_ids = ids(&base, &IdsFile::default());
        let table_uid = base_ids.table_uid(&old).unwrap().clone();
        let column_uid = base_ids.column_uid(&old.column("g")).unwrap().clone();
        let source_uid = base_ids.column_uid(&old.column("a")).unwrap().clone();
        let mut desired = base.clone();
        let mut definition = desired.tables.remove(&old).unwrap();
        let mut column = definition.columns.shift_remove("g").unwrap();
        column.generated.as_mut().unwrap().expression = "a * 3".into();
        definition.columns.insert(final_name.into(), column);
        desired.tables.insert(final_table.clone(), definition);
        let mut desired_ids = base_ids.clone();
        desired_ids.rename_table(&old, &final_table);
        desired_ids.columns.get_mut(&column_uid).unwrap().name = final_name.into();
        let opening = generation_records(&old, "g");
        let compiled = generation_records(&final_table, final_name);
        let mut changes = ChangeSet::default();
        if rename {
            changes.changes.extend([
                PlannedChange::new(Change::RenameTable {
                    uid: table_uid.clone(),
                    from: old.clone(),
                    to: final_table.clone(),
                    defaults: vec!["d".into()],
                }),
                PlannedChange::new(Change::RenameColumn {
                    uid: column_uid.clone(),
                    table: final_table.clone(),
                    from: "g".into(),
                    to: final_name.into(),
                    table_was: Some(old.clone()),
                }),
            ]);
        }
        changes
            .changes
            .push(PlannedChange::new(Change::AlterColumnExpression {
                uid: column_uid.clone(),
                column: final_table.column(final_name),
                from: "a * 2 + 1".into(),
                to: "a * 3".into(),
            }));
        let derive = |changes: &ChangeSet, opening_ids: &IdsFile| {
            super::transitions::derive(
                changes,
                pbps_diff::Side {
                    schema: &base,
                    ids: opening_ids,
                },
                pbps_diff::Side {
                    schema: &desired,
                    ids: &desired_ids,
                },
                &opening,
                &compiled,
            )
        };
        let transitions = derive(&changes, &base_ids).unwrap();
        let expression = transitions
            .iter()
            .find(|transition| {
                transition.surface == Surface::Default(final_table.column(final_name))
            })
            .expect("SET EXPRESSION has its own exact attrdef transition");
        let [old_column, before, old_owner, old_reference] = generation_objects(&old, "g");
        let [new_column, after, new_owner, new_reference] =
            generation_objects(&final_table, final_name);
        assert_eq!(
            expression.before,
            BTreeSet::from([before, old_owner, old_reference])
        );
        assert_eq!(
            expression.after,
            BTreeSet::from([after, new_owner, new_reference])
        );
        assert!(!expression.before.contains(&old_column));
        assert!(!expression.after.contains(&new_column));
        for opening in [true, false] {
            let claimed: Vec<_> = transitions
                .iter()
                .flat_map(|transition| {
                    if opening {
                        &transition.before
                    } else {
                        &transition.after
                    }
                })
                .collect();
            assert_eq!(claimed.len(), claimed.iter().collect::<BTreeSet<_>>().len());
        }

        let mut missing = base_ids.clone();
        missing.columns.remove(&column_uid);
        assert!(matches!(derive(&changes, &missing), Err(Error::Binding(_))));
        // A valid UID for a different recorded column is also wrong authority.
        // It must not select that input column's empty attrdef inventory.
        for wrong in [table_uid, source_uid] {
            let mut invalid = changes.clone();
            let Change::AlterColumnExpression { uid, .. } =
                &mut invalid.changes.last_mut().unwrap().change
            else {
                unreachable!()
            };
            *uid = wrong;
            assert!(matches!(
                derive(&invalid, &base_ids),
                Err(Error::Binding(_))
            ));
        }
    }
}

#[test]
fn create_and_add_stored_generation_keep_the_attrdef_child_inventory() {
    use pbps_model::PlannedChange;

    let table = pbps_model::TableName::new("app", "t");
    let desired = generation_schema(&table);
    let compiled = generation_records(&table, "g");
    for create in [true, false] {
        let mut base = Schema::default();
        if !create {
            base = desired.clone();
            base.tables
                .get_mut(&table)
                .unwrap()
                .columns
                .shift_remove("g");
        }
        let base_ids = ids(&base, &IdsFile::default());
        let desired_ids = ids(&desired, &base_ids);
        // ADD preserves a live parent on both sides. CREATE has no opening
        // parent; neither premise changes the generated child's inventory.
        let opening = if create {
            vec![]
        } else {
            generation_records(&table, "g")
                .into_iter()
                .filter(|record| {
                    !matches!(
                        &record.ownership,
                        ObjectOwnership::Surface(Surface::Column(c) | Surface::Default(c))
                            if c.name == "g"
                    )
                })
                .collect()
        };
        let (surface, change, expected) = if create {
            (
                Surface::Table(table.clone()),
                Change::CreateTable {
                    uid: desired_ids.table_uid(&table).unwrap().clone(),
                    name: table.clone(),
                    table: Box::new(desired.tables[&table].clone()),
                },
                compiled
                    .iter()
                    .map(|record| record.object.clone())
                    .collect(),
            )
        } else {
            let [column, attrdef, owner, reference] = generation_objects(&table, "g");
            (
                Surface::Column(table.column("g")),
                Change::AddColumn {
                    uid: desired_ids.column_uid(&table.column("g")).unwrap().clone(),
                    table: table.clone(),
                    name: "g".into(),
                    column: Box::new(desired.tables[&table].columns["g"].clone()),
                },
                BTreeSet::from([column, attrdef, owner, reference]),
            )
        };
        let transitions = super::transitions::derive(
            &ChangeSet {
                changes: vec![PlannedChange::new(change)],
            },
            pbps_diff::Side {
                schema: &base,
                ids: &base_ids,
            },
            pbps_diff::Side {
                schema: &desired,
                ids: &desired_ids,
            },
            &opening,
            &compiled,
        )
        .unwrap();
        assert_eq!(transitions.len(), if create { 1 } else { 2 });
        let child = transitions.iter().find(|t| t.surface == surface).unwrap();
        assert!(child.before.is_empty());
        assert_eq!(child.after, expected);
        if !create {
            let parent = transitions
                .iter()
                .find(|t| t.surface == Surface::Table(table.clone()))
                .unwrap();
            let relation = generation_objects(&table, "g")[0].signature[0].clone();
            assert_eq!(parent.before, BTreeSet::from([relation.clone()]));
            assert_eq!(parent.after, BTreeSet::from([relation]));
        }
    }
}

#[test]
fn generated_creation_surfaces_require_unique_qualified_attrdef_records() {
    use pbps_pg::resolver::capture::Assessment;

    let table = pbps_model::TableName::new("app", "t");
    let desired = generation_schema(&table);
    let compiled = generation_records(&table, "g");
    let [_, generated, ..] = generation_objects(&table, "g");
    let [_, ordinary, ..] = generation_objects(&table, "d");
    for create in [true, false] {
        let mut base = Schema::default();
        let mut opening = Vec::new();
        let mut assessment = Assessment::default();
        if !create {
            base = desired.clone();
            base.tables
                .get_mut(&table)
                .unwrap()
                .columns
                .shift_remove("g");
            let new_objects = BTreeSet::from(generation_objects(&table, "g"));
            opening = compiled
                .iter()
                .filter(|record| !new_objects.contains(&record.object))
                .cloned()
                .collect();
            assessment.surfaces.insert(
                ordinary.clone(),
                pbps_pg::resolver::capture::Verdict::Unaffected,
            );
        }
        let resolutions =
            super::resolution::from_records(&base, &desired, &opening, &compiled, &assessment)
                .unwrap();
        assert_eq!(resolutions.len(), 2);
        for (name, object) in [("g", &generated), ("d", &ordinary)] {
            let resolved = resolutions
                .iter()
                .find(|resolved| resolved.surface == Surface::Default(table.column(name)))
                .expect("ordinary defaults and generated expressions both require an attrdef");
            assert_eq!(resolved.current.is_some(), !create && name == "d");
            assert_eq!(&resolved.desired.as_ref().unwrap().object, object);
        }
    }

    for name in ["g", "d"] {
        let [_, attrdef, ..] = generation_objects(&table, name);
        for defect in ["missing", "duplicate", "unqualified", "reference-only"] {
            let mut records = compiled.clone();
            let index = records
                .iter()
                .position(|record| record.object == attrdef)
                .unwrap();
            match defect {
                "missing" => {
                    records.remove(index);
                }
                "duplicate" => {
                    records.push(records[index].clone());
                }
                "unqualified" => records[index].ownership = ObjectOwnership::Unqualified,
                "reference-only" => {
                    records[index].ownership =
                        ObjectOwnership::Surface(Surface::Column(table.column("a")));
                }
                _ => unreachable!(),
            }
            assert!(
                matches!(
                    super::resolution::from_records(
                        &Schema::default(),
                        &desired,
                        &[],
                        &records,
                        &Assessment::default(),
                    ),
                    Err(Error::Binding(_))
                ),
                "{name}: {defect} must refuse instead of omitting the required surface"
            );
        }
    }
}

#[test]
fn existing_generated_surfaces_require_both_records_and_a_resolved_binding_verdict() {
    use pbps_pg::resolver::capture::{Assessment, Verdict};

    let table = pbps_model::TableName::new("app", "t");
    let schema = generation_schema(&table);
    let records = generation_records(&table, "g");
    let [_, generated, ..] = generation_objects(&table, "g");
    let [_, ordinary, ..] = generation_objects(&table, "d");
    let assessment = |verdict| Assessment {
        surfaces: BTreeMap::from([
            (generated.clone(), verdict),
            (ordinary.clone(), Verdict::Unaffected),
        ]),
        ..Assessment::default()
    };
    for verdict in [Verdict::Unaffected, Verdict::Rebuild] {
        let resolutions = super::resolution::from_records(
            &schema,
            &schema,
            &records,
            &records,
            &assessment(verdict),
        )
        .unwrap();
        assert_eq!(resolutions.len(), 2);
        assert!(
            resolutions
                .iter()
                .all(|resolved| { resolved.current.is_some() && resolved.desired.is_some() })
        );
    }
    let mut no_verdict = assessment(Verdict::Unaffected);
    no_verdict.surfaces.remove(&generated);
    for verdict in [
        no_verdict,
        assessment(Verdict::Unresolved {
            condition: "test binding is unqualified",
        }),
    ] {
        assert!(matches!(
            super::resolution::from_records(&schema, &schema, &records, &records, &verdict),
            Err(Error::Binding(_))
        ));
    }
    let missing: Vec<_> = records
        .iter()
        .filter(|record| record.object != generated)
        .cloned()
        .collect();
    for opening in [true, false] {
        let (before, after) = if opening {
            (&missing, &records)
        } else {
            (&records, &missing)
        };
        assert!(matches!(
            super::resolution::from_records(
                &schema,
                &schema,
                before,
                after,
                &assessment(Verdict::Unaffected),
            ),
            Err(Error::Binding(_))
        ));
    }
}

/// The explicit ordered extra is an input to ordinary bootstrap, qualification,
/// scratch compilation and final planning, not a value inferred from the
/// target session's transient search_path.
#[tokio::test]
#[ignore = "requires pinned native PostgreSQL target and owned Docker fixture"]
async fn an_explicit_extra_schema_changes_only_the_unqualified_lookup_in_the_final_plan() {
    setup(cases::EXTRA_LOOKUP_TARGET_SETUP).await;
    let mut owned = Some(ObservedContainers::begin());
    let mut target = target().await;
    let mut run = open(Profile::Container, &mut target).await;
    let extras = vec!["pbps_evidence1274_extra".to_owned()];
    let inputs = Inputs::from_pair_with_extras(cases::extra_lookup_pair(), &extras);
    let key = ProjectKey::new(true);
    let result = run
        .plan_resolved(
            &mut target,
            &inputs.binding(),
            inputs.base(),
            inputs.desired(),
            &inputs.hints,
            &extras,
            &key.project,
            Some(ENVIRONMENT),
        )
        .await;
    let result = match result {
        Ok(result) => result,
        Err(error) => {
            close(&mut run, &mut owned).await;
            panic!("the qualified extra-schema lookup must be sealable: {error}");
        }
    };
    result.evidence.validate(&result.changes).unwrap();
    let affected = view(&result.evidence, "v");
    let control = view(&result.evidence, "control");
    assert_ne!(
        affected.current.as_ref().unwrap().bindings,
        affected.desired.as_ref().unwrap().bindings,
        "the earlier exact overload takes the unqualified call from the extra schema"
    );
    assert_eq!(
        control.current.as_ref().unwrap().bindings,
        control.desired.as_ref().unwrap().bindings,
        "the explicitly extra-qualified control keeps its original binding"
    );
    let v: pbps_model::ModuleId = "pbps_evidence1274.v".parse().unwrap();
    let control: pbps_model::ModuleId = "pbps_evidence1274.control".parse().unwrap();
    assert!(
        result
            .changes
            .changes
            .iter()
            .any(|step| { matches!(&step.change, Change::DropModule { id, .. } if id == &v) })
    );
    assert!(
        result
            .changes
            .changes
            .iter()
            .any(|step| { matches!(&step.change, Change::CreateModule { id, .. } if id == &v) })
    );
    assert!(!result.changes.changes.iter().any(|step| {
        matches!(&step.change, Change::DropModule { id, .. }
            | Change::CreateModule { id, .. }
            | Change::AlterModule { id, .. } if id == &control)
    }));
    close(&mut run, &mut owned).await;
    let mut peer = pbps_db::Conn::connect(
        Driver::Postgres,
        &std::env::var("PBPS_NATIVE_CONNECTION").unwrap(),
    )
    .await
    .unwrap();
    execute_plan_with_extras(&mut peer, &result.changes, &extras).await;
    let rows = peer
        .query(
            "SELECT (SELECT x FROM pbps_evidence1274.v) AS affected, \
                (SELECT x FROM pbps_evidence1274.control) AS control",
        )
        .await
        .unwrap();
    assert_eq!(rows[0].try_get::<i32>("affected").unwrap(), Some(30));
    assert_eq!(rows[0].try_get::<i32>("control").unwrap(), Some(10));
    drop(peer);
    target.check().await.unwrap();
    setup(&[]).await;
}

/// A rename does not make a later CREATE at the old spelling inherit the old
/// UID. The analogous added u.id must not inherit the renamed u.n column UID.
#[tokio::test]
#[ignore = "requires pinned native PostgreSQL target and owned Docker fixture"]
async fn recorded_renames_and_reused_old_spellings_keep_distinct_owned_inventories() {
    use pbps_db::fingerprint::EnvironmentFingerprintKey;

    setup(cases::RENAME_TARGET_SETUP).await;
    let opening_acl = table_column_grants("t", "id").await;
    let mut owned = Some(ObservedContainers::begin());
    let mut target = target().await;
    let mut run = open(Profile::Container, &mut target).await;
    let (base, desired, base_ids, desired_ids) = cases::rename_and_reuse_pair();
    let old: pbps_model::TableName = "pbps_evidence1274.t".parse().unwrap();
    let new: pbps_model::TableName = "pbps_evidence1274.u".parse().unwrap();
    assert_eq!(base_ids.table_uid(&old), desired_ids.table_uid(&new));
    assert_ne!(base_ids.table_uid(&old), desired_ids.table_uid(&old));
    assert_eq!(
        base_ids.column_uid(&old.column("id")),
        desired_ids.column_uid(&new.column("n"))
    );
    assert_ne!(
        base_ids.column_uid(&old.column("id")),
        desired_ids.column_uid(&new.column("id"))
    );
    assert_ne!(
        desired_ids.column_uid(&old.column("id")),
        desired_ids.column_uid(&new.column("id"))
    );
    let inputs = Inputs::with_ids(base, desired, base_ids, desired_ids);
    let key = ProjectKey::new(true);
    let result = run
        .plan_resolved(
            &mut target,
            &inputs.binding(),
            inputs.base(),
            inputs.desired(),
            &inputs.hints,
            &[],
            &key.project,
            Some(ENVIRONMENT),
        )
        .await;
    let result = match result {
        Ok(result) => result,
        Err(error) => {
            close(&mut run, &mut owned).await;
            panic!("renamed identities and their reused spellings must remain distinct: {error}");
        }
    };
    result.evidence.validate(&result.changes).unwrap();
    let changes = &result.changes.changes;
    let rename = changes
        .iter()
        .position(|step| {
            matches!(&step.change, Change::RenameTable { from, to, .. }
            if from == &old && to == &new)
        })
        .expect("the recorded old table is renamed to u");
    let create = changes
        .iter()
        .position(|step| {
            matches!(&step.change, Change::CreateTable { name, .. }
            if name == &old)
        })
        .expect("the old table spelling is reused by a new table");
    assert!(
        rename < create,
        "the old table name must be free before its reuse"
    );
    assert!(changes.iter().any(
        |step| matches!(&step.change, Change::AddColumn { table, name, .. }
        if table == &new && name == "id")
    ));

    let before = result.evidence.before();
    let after = result.evidence.after();
    let opening_owner = ObjectOwnership::Surface(Surface::Table(old.clone()));
    assert!(
        before
            .prerequisites()
            .iter()
            .any(|row| row.object.class == "pg_class"
                && row.object.name == [cases::SCHEMA, "t"]
                && row.ownership == opening_owner)
    );
    for (name, owner) in [
        ("u", Surface::Table(new.clone())),
        ("t", Surface::Table(old.clone())),
    ] {
        assert!(
            after
                .prerequisites()
                .iter()
                .any(|row| row.object.class == "pg_class"
                    && row.object.name == [cases::SCHEMA, name]
                    && row.ownership == ObjectOwnership::Surface(owner.clone())),
            "closing manifest must retain separate owned table inventories for {name}"
        );
    }
    for column in [new.column("n"), new.column("id"), old.column("id")] {
        assert!(
            after
                .prerequisites()
                .iter()
                .any(|row| row.object.class == "column"
                    && row.ownership == ObjectOwnership::Surface(Surface::Column(column.clone()))),
            "closing manifest must own a distinct catalog column: {column:?}"
        );
    }
    let closing = after.clone();
    close(&mut run, &mut owned).await;
    let mut peer = pbps_db::Conn::connect(
        Driver::Postgres,
        &std::env::var("PBPS_NATIVE_CONNECTION").unwrap(),
    )
    .await
    .unwrap();
    execute_plan(&mut peer, &result.changes).await;
    drop(peer);
    let selected = EnvironmentFingerprintKey::from_file(&key.root.join("key")).unwrap();
    let (_, observed) = target
        .capture_postgres_sealed(
            &catalog_scope(closing.scope()),
            &Default::default(),
            &selected,
        )
        .await
        .unwrap();
    for expected in closing.prerequisites() {
        let actual = observed
            .prerequisites()
            .iter()
            .find(|row| row.object == expected.object)
            .expect("both renamed and newly created records exist after DDL");
        assert_eq!(expected.properties, actual.properties);
        assert_eq!(expected.bindings, actual.bindings);
    }
    assert_eq!(table_column_grants("u", "n").await, opening_acl);
    let mut peer = PeerVerifiedConn::connect(
        Driver::Postgres,
        &std::env::var("PBPS_NATIVE_CONNECTION").unwrap(),
    )
    .await
    .unwrap();
    peer.query("INSERT INTO pbps_evidence1274.u DEFAULT VALUES")
        .await
        .unwrap();
    peer.query("INSERT INTO pbps_evidence1274.t (id) VALUES (7)")
        .await
        .unwrap();
    let rows = peer
        .query("SELECT n FROM pbps_evidence1274.u")
        .await
        .unwrap();
    assert_eq!(rows[0].try_get::<i32>("n").unwrap(), Some(9));
    let rows = peer
        .query("SELECT id FROM pbps_evidence1274.t")
        .await
        .unwrap();
    assert_eq!(rows[0].try_get::<i32>("id").unwrap(), Some(7));
    drop(peer);
    target.check().await.unwrap();
    setup(&[]).await;
}

/// A persisted authorization condition must survive a second process using
/// the same configured environment key. A different key must not reproduce
/// either fingerprint for the same fresh target and final typed plan.
#[tokio::test]
#[ignore = "requires pinned native PostgreSQL target and owned Docker fixture"]
async fn persisted_authorization_is_stable_across_processes_only_with_the_same_environment_key() {
    const TEST: &str = "resolver::server::qualified_evidence_tests::persisted_authorization_is_stable_across_processes_only_with_the_same_environment_key";

    async fn observe(project: &pbps_config::Project) -> Value {
        setup(cases::TARGET_SETUP).await;
        let mut owned = Some(ObservedContainers::begin());
        let mut target = target().await;
        let mut run = open(Profile::Container, &mut target).await;
        let inputs = Inputs::overload();
        let result = run
            .plan_resolved(
                &mut target,
                &inputs.binding(),
                inputs.base(),
                inputs.desired(),
                &inputs.hints,
                &[],
                project,
                Some(ENVIRONMENT),
            )
            .await;
        let result = match result {
            Ok(result) => result,
            Err(error) => {
                close(&mut run, &mut owned).await;
                panic!("fixed-key producer must qualify the same target: {error}");
            }
        };
        result.evidence.validate(&result.changes).unwrap();
        let authorization = serde_json::to_value(result.evidence.authorization()).unwrap();
        close(&mut run, &mut owned).await;
        target.check().await.unwrap();
        setup(&[]).await;
        authorization
    }

    if let Ok(kind) = std::env::var("PBPS_1274_AUTH_CHILD") {
        let output = PathBuf::from(std::env::var("PBPS_1274_AUTH_RESULT").unwrap());
        let authorization = match kind.as_str() {
            "same" => {
                let root = PathBuf::from(std::env::var("PBPS_1274_AUTH_ROOT").unwrap());
                let path = root.join("pbps.yml");
                let config = std::fs::read_to_string(&path).unwrap();
                let project = pbps_config::Project {
                    root,
                    config: pbps_config::Config::parse(&config, &path).unwrap(),
                };
                observe(&project).await
            }
            "different" => {
                let key = ProjectKey::new(true);
                observe(&key.project).await
            }
            other => panic!("unknown child authorization observation: {other}"),
        };
        std::fs::write(output, serde_json::to_vec(&authorization).unwrap()).unwrap();
        return;
    }

    let key = ProjectKey::new(true);
    let original = observe(&key.project).await;
    let binary = std::env::current_exe().unwrap();
    for kind in ["same", "different"] {
        let output = key.root.join(format!("authorization-{kind}.json"));
        let child = Command::new(&binary)
            .args(["--ignored", "--exact", TEST, "--nocapture"])
            .env("PBPS_1274_AUTH_CHILD", kind)
            .env("PBPS_1274_AUTH_RESULT", &output)
            .env("PBPS_1274_AUTH_ROOT", &key.root)
            .output()
            .unwrap();
        assert!(
            child.status.success()
                && String::from_utf8_lossy(&child.stdout).contains("test result: ok. 1 passed"),
            "{kind} key child process failed: stdout={} stderr={}",
            String::from_utf8_lossy(&child.stdout),
            String::from_utf8_lossy(&child.stderr)
        );
        let observed: Value = serde_json::from_slice(&std::fs::read(&output).unwrap()).unwrap();
        for phase in ["before", "after"] {
            if kind == "same" {
                assert_eq!(observed[phase], original[phase]);
            } else {
                assert_ne!(observed[phase], original[phase]);
            }
        }
    }
}

async fn not_null_oid(table: &str, constraint: &str) -> Option<i64> {
    // Only fixed names from the disposable #1274 fixture reach this query.
    assert!(matches!(table, "type_case" | "type_final"));
    assert_eq!(constraint, "type_case_id_not_null");
    let mut peer = PeerVerifiedConn::connect(
        Driver::Postgres,
        &std::env::var("PBPS_NATIVE_CONNECTION").unwrap(),
    )
    .await
    .unwrap();
    let rows = peer
        .query(&format!(
            "SELECT c.oid::bigint AS oid FROM pg_catalog.pg_constraint c \
             JOIN pg_catalog.pg_class t ON t.oid = c.conrelid \
             JOIN pg_catalog.pg_namespace n ON n.oid = t.relnamespace \
             WHERE n.nspname = 'pbps_evidence1274' \
               AND t.relname = '{table}' AND c.conname = '{constraint}'"
        ))
        .await
        .unwrap();
    assert!(rows.len() <= 1);
    rows.first()
        .map(|row| row.try_get::<i64>("oid").unwrap().unwrap())
}

/// The same recorded table/column renames accompany three different typed
/// edits. PG18 retains a NOT NULL name through a type rewrite despite a new
/// raw constraint OID, removes it on DROP, and makes the final name on SET.
/// PG16 has no separate constraint rows. Projected closing properties must
/// agree with a fresh actual target capture after the emitted plan runs.
#[tokio::test]
#[ignore = "requires pinned native PostgreSQL target and owned Docker fixture"]
async fn recorded_renames_with_type_and_nullability_edits_match_actual_child_catalog() {
    use pbps_db::fingerprint::EnvironmentFingerprintKey;

    let major: u32 = std::env::var("PBPS_NATIVE_PG_MAJOR")
        .unwrap()
        .parse()
        .unwrap();
    assert!(matches!(major, 16 | 18));
    setup(cases::TYPED_RENAME_TARGET_SETUP).await;
    let opening_oid = not_null_oid("type_case", "type_case_id_not_null").await;
    assert_eq!(opening_oid.is_some(), major == 18);

    let mut owned = Some(ObservedContainers::begin());
    let mut target = target().await;
    let mut run = open(Profile::Container, &mut target).await;
    let (base, desired, base_ids, desired_ids) = cases::typed_rename_pair();
    for stem in ["type", "drop", "add"] {
        let old: pbps_model::TableName = format!("pbps_evidence1274.{stem}_case").parse().unwrap();
        let new: pbps_model::TableName = format!("pbps_evidence1274.{stem}_final").parse().unwrap();
        assert_eq!(base_ids.table_uid(&old), desired_ids.table_uid(&new));
        assert_eq!(
            base_ids.column_uid(&old.column("id")),
            desired_ids.column_uid(&new.column("n"))
        );
    }
    let inputs = Inputs::with_ids(base, desired, base_ids, desired_ids);
    let key = ProjectKey::new(true);
    let result = run
        .plan_resolved(
            &mut target,
            &inputs.binding(),
            inputs.base(),
            inputs.desired(),
            &inputs.hints,
            &[],
            &key.project,
            Some(ENVIRONMENT),
        )
        .await;
    let result = match result {
        Ok(result) => result,
        Err(error) => {
            close(&mut run, &mut owned).await;
            panic!("measured typed renames must produce qualified evidence: {error}");
        }
    };
    close(&mut run, &mut owned).await;
    result.evidence.validate(&result.changes).unwrap();

    let changes = &result.changes.changes;
    for stem in ["type", "drop", "add"] {
        let old: pbps_model::TableName = format!("pbps_evidence1274.{stem}_case").parse().unwrap();
        let new: pbps_model::TableName = format!("pbps_evidence1274.{stem}_final").parse().unwrap();
        assert!(changes.iter().any(|step| matches!(
            &step.change,
            Change::RenameTable { from, to, .. } if from == &old && to == &new
        )));
        assert!(changes.iter().any(|step| matches!(
            &step.change,
            Change::RenameColumn { table, from, to, .. }
                if table == &new && from == "id" && to == "n"
        )));
    }
    let typed = "pbps_evidence1274.type_final"
        .parse::<pbps_model::TableName>()
        .unwrap();
    let dropped = "pbps_evidence1274.drop_final"
        .parse::<pbps_model::TableName>()
        .unwrap();
    let added = "pbps_evidence1274.add_final"
        .parse::<pbps_model::TableName>()
        .unwrap();
    assert!(changes.iter().any(|step| matches!(
        &step.change,
        Change::AlterColumnType { column, .. } if column == &typed.column("n")
    )));
    for column in [dropped.column("n"), added.column("n")] {
        assert!(changes.iter().any(|step| matches!(
            &step.change,
            Change::AlterColumnNullability { column: changed, .. }
                if changed == &column
        )));
    }

    let closing = result.evidence.after().clone();
    // Properties are sealed HMACs. This fixture declares no other
    // constraints, so its pg_constraint identities enumerate the NOT NULL
    // children; the actual catalog query below also checks their contype.
    let child_identities: BTreeSet<_> = closing
        .prerequisites()
        .iter()
        .filter(|row| row.object.class == "pg_constraint")
        .map(|row| {
            let relation = row.object.signature.get(1).unwrap();
            assert_eq!(relation.class, "pg_class");
            assert_eq!(
                relation.name.first().map(String::as_str),
                Some(cases::SCHEMA)
            );
            (
                relation.name.get(1).unwrap().clone(),
                row.object.name.first().unwrap().clone(),
            )
        })
        .collect();
    let expected_children: BTreeSet<_> = if major == 18 {
        [
            ("type_final", "type_case_id_not_null"),
            ("add_final", "add_final_n_not_null"),
        ]
        .into_iter()
        .map(|(table, name)| (table.to_owned(), name.to_owned()))
        .collect()
    } else {
        BTreeSet::new()
    };
    assert_eq!(child_identities, expected_children);

    let mut peer = pbps_db::Conn::connect(
        Driver::Postgres,
        &std::env::var("PBPS_NATIVE_CONNECTION").unwrap(),
    )
    .await
    .unwrap();
    execute_plan(&mut peer, &result.changes).await;
    drop(peer);
    let final_oid = not_null_oid("type_final", "type_case_id_not_null").await;
    assert_eq!(final_oid.is_some(), major == 18);
    if major == 18 {
        assert_ne!(opening_oid, final_oid, "the type rewrite replaces raw OID");
    }
    let mut peer = PeerVerifiedConn::connect(
        Driver::Postgres,
        &std::env::var("PBPS_NATIVE_CONNECTION").unwrap(),
    )
    .await
    .unwrap();
    let rows = peer
        .query(
            "SELECT t.relname::text AS table_name, c.conname::text AS constraint_name, \
                c.contype::text AS kind \
             FROM pg_catalog.pg_constraint c \
             JOIN pg_catalog.pg_class t ON t.oid = c.conrelid \
             JOIN pg_catalog.pg_namespace n ON n.oid = t.relnamespace \
             WHERE n.nspname = 'pbps_evidence1274' \
               AND t.relname IN ('type_final', 'drop_final', 'add_final')",
        )
        .await
        .unwrap();
    let actual_children: BTreeSet<_> = rows
        .iter()
        .map(|row| {
            assert_eq!(row.try_get::<&str>("kind").unwrap(), Some("n"));
            (
                row.try_get::<&str>("table_name")
                    .unwrap()
                    .unwrap()
                    .to_owned(),
                row.try_get::<&str>("constraint_name")
                    .unwrap()
                    .unwrap()
                    .to_owned(),
            )
        })
        .collect();
    assert_eq!(actual_children, expected_children);
    drop(peer);

    let selected = EnvironmentFingerprintKey::from_file(&key.root.join("key")).unwrap();
    let (_, observed) = target
        .capture_postgres_sealed(
            &catalog_scope(closing.scope()),
            &Default::default(),
            &selected,
        )
        .await
        .unwrap();
    for expected in closing.prerequisites() {
        let actual = observed
            .prerequisites()
            .iter()
            .find(|row| row.object == expected.object)
            .unwrap_or_else(|| panic!("missing actual closing identity: {:?}", expected.object));
        assert_eq!(expected.properties, actual.properties);
        assert_eq!(expected.bindings, actual.bindings);
    }
    let mut peer = PeerVerifiedConn::connect(
        Driver::Postgres,
        &std::env::var("PBPS_NATIVE_CONNECTION").unwrap(),
    )
    .await
    .unwrap();
    let rows = peer
        .query(
            "SELECT t.relname::text || ':' || a.attname || ':' || \
                pg_catalog.format_type(a.atttypid, a.atttypmod) || ':' || \
                a.attnotnull::text AS state \
             FROM pg_catalog.pg_attribute a \
             JOIN pg_catalog.pg_class t ON t.oid = a.attrelid \
             JOIN pg_catalog.pg_namespace n ON n.oid = t.relnamespace \
             WHERE n.nspname = 'pbps_evidence1274' AND a.attnum > 0 \
               AND t.relname IN ('type_final', 'drop_final', 'add_final')",
        )
        .await
        .unwrap();
    let actual_states: BTreeSet<_> = rows
        .iter()
        .map(|row| row.try_get::<&str>("state").unwrap().unwrap().to_owned())
        .collect();
    let expected_states: BTreeSet<_> = [
        "type_final:n:bigint:true",
        "drop_final:n:integer:false",
        "add_final:n:integer:true",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect();
    assert_eq!(actual_states, expected_states);
    drop(peer);
    target.check().await.unwrap();
    setup(&[]).await;
}

/// A newly created routine follows the target connection's effective creator.
/// A preserved ordinary table owner does not exercise this creation branch.
#[tokio::test]
#[ignore = "requires pinned native PostgreSQL target and owned Docker fixture"]
async fn a_new_routine_created_as_an_ordinary_deployer_keeps_its_owner_edge() {
    use pbps_db::fingerprint::EnvironmentFingerprintKey;
    use pbps_db::resolver::capture::ObjectIdentity;
    use pbps_model::{Module, ModuleKind};

    setup(&[
        "CREATE SCHEMA pbps_evidence1274",
        "GRANT USAGE, CREATE ON SCHEMA pbps_evidence1274 TO pbps_native_alt",
    ])
    .await;
    let mut peer = PeerVerifiedConn::connect(
        Driver::Postgres,
        &std::env::var("PBPS_NATIVE_CONNECTION").unwrap(),
    )
    .await
    .unwrap();
    let roles = peer
        .query("SELECT oid::int8 AS oid FROM pg_catalog.pg_roles WHERE rolname = 'pbps_native_alt'")
        .await
        .unwrap();
    assert_eq!(roles.len(), 1);
    assert!(roles[0].try_get::<i64>("oid").unwrap().unwrap() >= 12_000);
    let settings_membership_sql = concat!(
        "SELECT EXISTS (",
        "SELECT 1 FROM pg_catalog.pg_auth_members m ",
        "JOIN pg_catalog.pg_roles granted ON granted.oid = m.roleid ",
        "JOIN pg_catalog.pg_roles member ON member.oid = m.member ",
        "WHERE granted.rolname = 'pg_read_all_settings' ",
        "AND member.rolname = 'pbps_native_alt') AS member"
    );
    let membership = peer.query(settings_membership_sql).await.unwrap();
    assert_eq!(membership.len(), 1);
    let had_settings_membership = membership[0].try_get::<bool>("member").unwrap().unwrap();
    eprintln!("ordinary_creator_settings prior_membership={had_settings_membership}");
    // The measured ordinary-role refusal had three unreadable settings:
    // dynamic_library_path, session_preload_libraries and shared_preload_libraries.
    // Give only this disposable target role the catalog-read privilege before
    // admission; keep the effective deployer ordinary throughout the case.
    if !had_settings_membership {
        peer.query("GRANT pg_read_all_settings TO pbps_native_alt")
            .await
            .unwrap();
    }
    // Qualification and the fresh catalog read must see this same role on the
    // bound connection; changing a separate observer would prove nothing.
    peer.query("SET ROLE pbps_native_alt").await.unwrap();
    let principal = peer
        .query(
            "SELECT current_user::text AS effective, session_user::text AS login, \
             pg_catalog.current_setting('is_superuser') AS superuser",
        )
        .await
        .unwrap();
    assert_eq!(principal.len(), 1);
    assert_eq!(
        principal[0].try_get::<&str>("effective").unwrap(),
        Some("pbps_native_alt")
    );
    assert_eq!(
        principal[0].try_get::<&str>("login").unwrap(),
        Some("postgres")
    );
    assert_eq!(
        principal[0].try_get::<&str>("superuser").unwrap(),
        Some("off")
    );
    let mut target = NativeTarget::establish(
        peer,
        std::env::var("PBPS_NATIVE_SERVICE_PID")
            .unwrap()
            .parse()
            .unwrap(),
    )
    .await
    .unwrap();
    let mut owned = Some(ObservedContainers::begin());
    let mut run = open(Profile::Container, &mut target).await;
    let base = Schema::default();
    let mut desired = Schema::default();
    desired.modules.insert(
        "pbps_evidence1274.a()".parse().unwrap(),
        Module {
            kind: ModuleKind::Function,
            description: None,
            definition: "() RETURNS integer LANGUAGE SQL IMMUTABLE RETURN 1".into(),
        },
    );
    let inputs = Inputs::from_pair((base, desired));
    let key = ProjectKey::new(true);
    let result = run
        .plan_resolved(
            &mut target,
            &inputs.binding(),
            inputs.base(),
            inputs.desired(),
            &inputs.hints,
            &[],
            &key.project,
            Some(ENVIRONMENT),
        )
        .await;
    // No assertion on the plan or catalog may strand the run's exact owners.
    close(&mut run, &mut owned).await;
    let result = result.expect("the ordinary effective creator can seal a new routine");
    result.evidence.validate(&result.changes).unwrap();
    assert!(result.changes.changes.iter().any(|step| {
        matches!(&step.change, Change::CreateModule { id, .. }
            if id.to_string() == "pbps_evidence1274.a()")
    }));
    let closing = result.evidence.after().clone();
    let routine = ObjectIdentity {
        class: "pg_proc".into(),
        name: vec![cases::SCHEMA.into(), "a".into()],
        signature: Vec::new(),
    };
    let owner_edge = ObjectIdentity {
        class: "pg_shdepend".into(),
        name: vec!["o".into()],
        signature: vec![
            routine.clone(),
            ObjectIdentity {
                class: "pg_authid".into(),
                name: vec!["pbps_native_alt".into()],
                signature: Vec::new(),
            },
        ],
    };
    assert_eq!(
        closing
            .prerequisites()
            .iter()
            .filter(|row| row.object == owner_edge)
            .count(),
        1,
        "the projected new routine has its ordinary creator's exact owner edge"
    );
    let mut writer = pbps_db::Conn::connect(
        Driver::Postgres,
        &std::env::var("PBPS_NATIVE_CONNECTION").unwrap(),
    )
    .await
    .unwrap();
    writer.execute("SET ROLE pbps_native_alt").await.unwrap();
    let applied_as = writer
        .query("SELECT current_user::text AS effective")
        .await
        .unwrap();
    assert_eq!(
        applied_as[0].try_get::<&str>("effective").unwrap(),
        Some("pbps_native_alt")
    );
    execute_plan(&mut writer, &result.changes).await;
    drop(writer);
    assert_eq!(
        catalog_owner_dependency("routine-a").await,
        ("pbps_native_alt".into(), 1, 1),
        "actual creation as the ordinary deployer records one owner edge"
    );
    let selected = EnvironmentFingerprintKey::from_file(&key.root.join("key")).unwrap();
    let (_, observed) = target
        .capture_postgres_sealed(
            &catalog_scope(closing.scope()),
            &Default::default(),
            &selected,
        )
        .await
        .unwrap();
    assert_eq!(
        closing.prerequisites().len(),
        observed.prerequisites().len(),
        "the closing inventory has exactly the fresh target's members"
    );
    for (expected, actual) in closing.prerequisites().iter().zip(observed.prerequisites()) {
        assert_eq!(expected.object, actual.object);
        assert_eq!(
            expected.properties, actual.properties,
            "projected closing properties differ for {:?}",
            expected.object
        );
        assert_eq!(
            expected.bindings, actual.bindings,
            "projected closing bindings differ for {:?}",
            expected.object
        );
    }
    target.check().await.unwrap();
    let mut admin = PeerVerifiedConn::connect(
        Driver::Postgres,
        &std::env::var("PBPS_NATIVE_CONNECTION").unwrap(),
    )
    .await
    .unwrap();
    if !had_settings_membership {
        admin
            .query("REVOKE pg_read_all_settings FROM pbps_native_alt")
            .await
            .unwrap();
    }
    let restored = admin.query(settings_membership_sql).await.unwrap();
    assert_eq!(restored.len(), 1);
    assert_eq!(
        restored[0].try_get::<bool>("member").unwrap(),
        Some(had_settings_membership),
        "the disposable role's previous settings membership is restored"
    );
    eprintln!("ordinary_creator_settings restored_membership={had_settings_membership}");
    drop(admin);
    setup(&[]).await;
}

/// A verified scope belongs to the same preliminary plan and both live
/// connections. Reusing precisely that request must still seal evidence.
#[tokio::test]
#[ignore = "requires pinned native PostgreSQL target and owned Docker fixture"]
async fn a_prequalified_matching_scope_seals_the_same_ordinary_plan() {
    setup(cases::TARGET_SETUP).await;
    let inputs = Inputs::overload();
    let key = ProjectKey::new(true);
    let ordinary = pbps_diff::diff(
        inputs.base(),
        inputs.desired(),
        &pbps_pg::Postgres::with_write_path_extras(vec![]),
        &inputs.hints,
    )
    .unwrap();
    assert!(scope::planned_schema_grants(&ordinary).unwrap().is_empty());
    let mut owned = Some(ObservedContainers::begin());
    let mut target = target().await;
    let mut run = open(Profile::Container, &mut target).await;
    let qualified = run
        .qualify(
            &mut target,
            &ScopeRequest {
                schemas: vec![cases::SCHEMA.into()],
                write_path_extras: Vec::new(),
                planned: Vec::new(),
            },
        )
        .await;
    if !matches!(&qualified, Ok(Verdict::Verified)) {
        close(&mut run, &mut owned).await;
        setup(&[]).await;
        panic!("the ordinary preliminary request must truly verify: {qualified:?}");
    }
    let before = run.scope_connections();
    let result = run
        .plan_resolved(
            &mut target,
            &inputs.binding(),
            inputs.base(),
            inputs.desired(),
            &inputs.hints,
            &[],
            &key.project,
            Some(ENVIRONMENT),
        )
        .await;
    let after = run.scope_connections();
    let compiled = run.compiled;
    close(&mut run, &mut owned).await;
    setup(&[]).await;
    assert!(before.is_some());
    assert_eq!(after, before);
    assert!(compiled, "the matching plan reached the one-shot compiler");
    let resolved = result.expect("the same qualified request seals its ordinary plan");
    resolved.evidence.validate(&resolved.changes).unwrap();
}

/// The two extras name the same schemas, but their search order selects the
/// unqualified routine. A prior qualification must not authorize a reorder.
#[tokio::test]
#[ignore = "requires pinned native PostgreSQL target and owned Docker fixture"]
async fn a_prequalified_scope_rejects_reordered_write_path_extras_before_compilation() {
    setup(cases::EXTRA_LOOKUP_TARGET_SETUP).await;
    let qualified_extras = vec!["pbps_evidence1274_extra".into(), "public".into()];
    let requested_extras = vec!["public".into(), "pbps_evidence1274_extra".into()];
    let inputs = Inputs::from_pair_with_extras(cases::extra_lookup_pair(), &requested_extras);
    let key = ProjectKey::new(true);
    let ordinary = pbps_diff::diff(
        inputs.base(),
        inputs.desired(),
        &pbps_pg::Postgres::with_write_path_extras(requested_extras.clone()),
        &inputs.hints,
    )
    .unwrap();
    assert!(scope::planned_schema_grants(&ordinary).unwrap().is_empty());
    let mut owned = Some(ObservedContainers::begin());
    let mut target = target().await;
    let mut run = open(Profile::Container, &mut target).await;
    let qualified = run
        .qualify(
            &mut target,
            &ScopeRequest {
                schemas: vec![cases::SCHEMA.into(), "pbps_evidence1274_extra".into()],
                write_path_extras: qualified_extras,
                planned: Vec::new(),
            },
        )
        .await;
    if !matches!(&qualified, Ok(Verdict::Verified)) {
        close(&mut run, &mut owned).await;
        setup(&[]).await;
        panic!("the original ordered extra scope must truly verify: {qualified:?}");
    }
    let before = run.scope_connections();
    let result = run
        .plan_resolved(
            &mut target,
            &inputs.binding(),
            inputs.base(),
            inputs.desired(),
            &inputs.hints,
            &requested_extras,
            &key.project,
            Some(ENVIRONMENT),
        )
        .await;
    let after = run.scope_connections();
    let compiled = run.compiled;
    close(&mut run, &mut owned).await;
    setup(&[]).await;
    assert!(before.is_some());
    assert_eq!(after, before, "an early refusal cannot rebind the scope");
    assert!(!compiled, "a reordered write path cannot start compilation");
    assert!(matches!(result, Err(Error::Scope(ref reason))
        if reason == "the existing verified scope does not match the planning request"));
}

/// The opening qualification may project a planned schema grant, but the
/// ordinary overload plan performs none. That old scope cannot seal it.
#[tokio::test]
#[ignore = "requires pinned native PostgreSQL target and owned Docker fixture"]
async fn a_prequalified_scope_rejects_changed_preliminary_grants_before_compilation() {
    setup(cases::TARGET_SETUP).await;
    let inputs = Inputs::overload();
    let key = ProjectKey::new(true);
    let ordinary = pbps_diff::diff(
        inputs.base(),
        inputs.desired(),
        &pbps_pg::Postgres::with_write_path_extras(vec![]),
        &inputs.hints,
    )
    .unwrap();
    assert!(scope::planned_schema_grants(&ordinary).unwrap().is_empty());
    let mut owned = Some(ObservedContainers::begin());
    let mut target = target().await;
    let mut run = open(Profile::Container, &mut target).await;
    let qualified = run
        .qualify(
            &mut target,
            &ScopeRequest {
                schemas: vec![cases::SCHEMA.into()],
                write_path_extras: Vec::new(),
                planned: vec![PlannedGrant {
                    principal: "PUBLIC".into(),
                    schema: cases::SCHEMA.into(),
                    privilege: "USAGE".into(),
                    revoke: false,
                }],
            },
        )
        .await;
    if !matches!(&qualified, Ok(Verdict::Verified)) {
        close(&mut run, &mut owned).await;
        setup(&[]).await;
        panic!("the planned PUBLIC USAGE scope must truly verify: {qualified:?}");
    }
    let before = run.scope_connections();
    let result = run
        .plan_resolved(
            &mut target,
            &inputs.binding(),
            inputs.base(),
            inputs.desired(),
            &inputs.hints,
            &[],
            &key.project,
            Some(ENVIRONMENT),
        )
        .await;
    let after = run.scope_connections();
    let compiled = run.compiled;
    close(&mut run, &mut owned).await;
    setup(&[]).await;
    assert!(before.is_some());
    assert_eq!(after, before, "an early refusal cannot rebind the scope");
    assert!(
        !compiled,
        "a changed grant sequence cannot start compilation"
    );
    assert!(matches!(result, Err(Error::Scope(ref reason))
        if reason == "the existing verified scope does not match the planning request"));
}

fn review_table() -> pbps_model::Table {
    let mut table = pbps_model::Table::default();
    table.columns.insert(
        "n".into(),
        pbps_model::Column::new("integer".parse().unwrap()).not_null(),
    );
    table
}

async fn table_catalog_flags() -> (i64, bool) {
    let mut peer = PeerVerifiedConn::connect(
        Driver::Postgres,
        &std::env::var("PBPS_NATIVE_CONNECTION").unwrap(),
    )
    .await
    .unwrap();
    let rows = peer
        .query(
            "SELECT c.relchecks::int8 AS checks, c.relhasindex AS has_index \
             FROM pg_catalog.pg_class c \
             JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
             WHERE n.nspname = 'pbps_evidence1274' AND c.relname = 't'",
        )
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    (
        rows[0].try_get::<i64>("checks").unwrap().unwrap(),
        rows[0].try_get::<bool>("has_index").unwrap().unwrap(),
    )
}

async fn assert_review_closing_matches_target(
    target: &mut NativeTarget,
    closing: &pbps_model::resolver::InputManifest,
    key: &ProjectKey,
) -> pbps_model::resolver::InputManifest {
    let selected =
        pbps_db::fingerprint::EnvironmentFingerprintKey::from_file(&key.root.join("key")).unwrap();
    let (_, observed) = target
        .capture_postgres_sealed(
            &catalog_scope(closing.scope()),
            &Default::default(),
            &selected,
        )
        .await
        .unwrap();
    for expected in closing.prerequisites() {
        let actual = observed
            .prerequisites()
            .iter()
            .find(|row| row.object == expected.object)
            .unwrap_or_else(|| panic!("post-DDL catalog omitted {:?}", expected.object));
        assert_eq!(
            expected.properties, actual.properties,
            "post-DDL properties differ for {:?}",
            expected.object
        );
        assert_eq!(
            expected.bindings, actual.bindings,
            "post-DDL bindings differ for {:?}",
            expected.object
        );
    }
    observed
}

async fn review_plan(
    target: &mut NativeTarget,
    run: &mut ScratchRun,
    owned: &mut Option<ObservedContainers>,
    inputs: &Inputs,
    key: &ProjectKey,
) -> ResolvedPlan {
    let result = run
        .plan_resolved(
            target,
            &inputs.binding(),
            inputs.base(),
            inputs.desired(),
            &inputs.hints,
            &[],
            &key.project,
            Some(ENVIRONMENT),
        )
        .await;
    close(run, owned).await;
    let result = result.expect("the review fixture's typed plan is valid");
    result.evidence.validate(&result.changes).unwrap();
    result
}

/// ADD CONSTRAINT changes the table in place. Its target owner and ACLs are
/// not replaced by those of the scratch table used to compile the plan.
async fn key_check_case(include_key: bool) {
    setup(&[
        "CREATE SCHEMA pbps_evidence1274",
        "CREATE TABLE pbps_evidence1274.t (n integer NOT NULL)",
        "ALTER TABLE pbps_evidence1274.t OWNER TO pbps_native_alt",
        "GRANT SELECT ON pbps_evidence1274.t TO pg_monitor WITH GRANT OPTION",
        "GRANT UPDATE(n) ON pbps_evidence1274.t TO pg_monitor WITH GRANT OPTION",
    ])
    .await;
    let opening_acl = table_column_grants("t", "n").await;
    assert_eq!(table_catalog_flags().await, (0, false));
    let opening_owner = catalog_owner_dependency("table-t").await;
    assert_eq!(opening_owner, ("pbps_native_alt".into(), 1, 1));
    let mut base = Schema::default();
    base.tables
        .insert("pbps_evidence1274.t".parse().unwrap(), review_table());
    base.roles
        .insert("pbps_native_alt".into(), pbps_model::Role::default());
    let mut desired = base.clone();
    let table = desired
        .tables
        .get_mut(&"pbps_evidence1274.t".parse().unwrap())
        .unwrap();
    if include_key {
        table.primary_key = Some(pbps_model::PrimaryKey {
            name: Some("t_named_pk".into()),
            columns: vec!["n".into()],
        });
    }
    table.checks.insert(
        "n_positive".into(),
        pbps_model::CheckConstraint {
            expression: "n >= 0".into(),
        },
    );
    let inputs = Inputs::from_pair((base, desired));
    let key = ProjectKey::new(true);
    let mut owned = Some(ObservedContainers::begin());
    let mut target = target().await;
    let mut run = open(Profile::Container, &mut target).await;
    let result = review_plan(&mut target, &mut run, &mut owned, &inputs, &key).await;
    assert_eq!(
        result
            .changes
            .changes
            .iter()
            .any(|step| matches!(step.change, Change::SetPrimaryKey { .. })),
        include_key
    );
    assert!(
        result
            .changes
            .changes
            .iter()
            .any(|step| matches!(step.change, Change::AddCheck { .. }))
    );
    assert!(!result.changes.changes.iter().any(|step| matches!(
        step.change,
        Change::CreateTable { .. } | Change::DropTable { .. } | Change::RenameTable { .. }
    )));
    let closing = result.evidence.after().clone();
    let mut peer = pbps_db::Conn::connect(
        Driver::Postgres,
        &std::env::var("PBPS_NATIVE_CONNECTION").unwrap(),
    )
    .await
    .unwrap();
    execute_plan(&mut peer, &result.changes).await;
    drop(peer);
    assert_eq!(table_column_grants("t", "n").await, opening_acl);
    assert_eq!(catalog_owner_dependency("table-t").await, opening_owner);
    assert_eq!(table_catalog_flags().await, (1, include_key));
    assert_review_closing_matches_target(&mut target, &closing, &key).await;
    target.check().await.unwrap();
    setup(&[]).await;
}

#[tokio::test]
#[ignore = "requires pinned native PostgreSQL target and owned Docker fixture"]
async fn adding_named_key_and_check_keeps_existing_table_owner_and_grants() {
    key_check_case(true).await;
}

#[tokio::test]
#[ignore = "requires pinned native PostgreSQL target and owned Docker fixture"]
async fn adding_check_alone_keeps_existing_table_owner_and_grants() {
    key_check_case(false).await;
}

/// The new typed grant and the unrelated old grant must both survive. A
/// blanket opening-ACL copy loses the new grant; scratch-only ACLs lose the
/// target's independently granted pg_monitor privileges.
#[tokio::test]
#[ignore = "requires pinned native PostgreSQL target and owned Docker fixture"]
async fn granting_on_an_existing_table_keeps_old_acl_and_adds_the_declared_role() {
    setup(&[
        "CREATE SCHEMA pbps_evidence1274",
        "CREATE TABLE pbps_evidence1274.t (n integer NOT NULL)",
        "ALTER TABLE pbps_evidence1274.t OWNER TO pbps_native_alt",
        "GRANT SELECT ON pbps_evidence1274.t TO pg_monitor WITH GRANT OPTION",
        "GRANT UPDATE(n) ON pbps_evidence1274.t TO pg_monitor WITH GRANT OPTION",
    ])
    .await;
    let opening_acl = table_column_grants("t", "n").await;
    assert_eq!(
        catalog_owner_dependency("table-t").await,
        ("pbps_native_alt".into(), 1, 1)
    );
    let mut base = Schema::default();
    base.tables
        .insert("pbps_evidence1274.t".parse().unwrap(), review_table());
    base.roles
        .insert("pbps_native_alt".into(), pbps_model::Role::default());
    base.roles
        .insert("pg_monitor".into(), pbps_model::Role::default());
    let mut desired = base.clone();
    desired.roles.get_mut("pg_monitor").unwrap().grants.insert(
        "pbps_evidence1274.t".parse().unwrap(),
        BTreeSet::from([pbps_model::Permission::Update]),
    );
    let inputs = Inputs::from_pair((base, desired));
    let key = ProjectKey::new(true);
    let mut owned = Some(ObservedContainers::begin());
    let mut target = target().await;
    let mut run = open(Profile::Container, &mut target).await;
    let result = review_plan(&mut target, &mut run, &mut owned, &inputs, &key).await;
    assert!(result.changes.changes.iter().any(|step| matches!(
        &step.change,
        Change::Grant { role, target, permissions }
            if role == "pg_monitor"
                && target.to_string() == "pbps_evidence1274.t"
                && permissions == &BTreeSet::from([pbps_model::Permission::Update])
    )));
    assert!(!result.changes.changes.iter().any(|step| matches!(
        step.change,
        Change::CreateTable { .. } | Change::DropTable { .. } | Change::RenameTable { .. }
    )));
    let closing = result.evidence.after().clone();
    let mut peer = pbps_db::Conn::connect(
        Driver::Postgres,
        &std::env::var("PBPS_NATIVE_CONNECTION").unwrap(),
    )
    .await
    .unwrap();
    execute_plan(&mut peer, &result.changes).await;
    drop(peer);
    let after_acl = table_column_grants("t", "n").await;
    assert_eq!(after_acl.0, opening_acl.0);
    assert_eq!(after_acl.2, opening_acl.2);
    let mut observer = PeerVerifiedConn::connect(
        Driver::Postgres,
        &std::env::var("PBPS_NATIVE_CONNECTION").unwrap(),
    )
    .await
    .unwrap();
    let grants = observer
        .query(
            "SELECT a.grantee = 'pg_monitor'::regrole AS old_grantee, \
                    a.privilege_type = 'UPDATE' AS new_permission, \
                    pg_catalog.pg_get_userbyid(a.grantor)::text AS grantor, \
                    a.is_grantable AS grant_option \
             FROM pg_catalog.pg_class c \
             JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
             CROSS JOIN LATERAL pg_catalog.aclexplode(c.relacl) a \
             WHERE n.nspname = 'pbps_evidence1274' AND c.relname = 't' \
               AND a.privilege_type IN ('SELECT', 'UPDATE')",
        )
        .await
        .unwrap();
    assert_eq!(
        grants
            .iter()
            .filter(
                |row| row.try_get::<bool>("old_grantee").unwrap() == Some(true)
                    && row.try_get::<bool>("new_permission").unwrap() == Some(false)
                    && row.try_get::<bool>("grant_option").unwrap() == Some(true)
            )
            .count(),
        1,
        "the undeclared opening grant and its option survive"
    );
    assert_eq!(
        grants
            .iter()
            .filter(
                |row| row.try_get::<bool>("old_grantee").unwrap() == Some(true)
                    && row.try_get::<bool>("new_permission").unwrap() == Some(true)
                    && row.try_get::<bool>("grant_option").unwrap() == Some(false)
                    && row.try_get::<&str>("grantor").unwrap() == Some("pbps_native_alt")
            )
            .count(),
        1,
        "the typed UPDATE is granted once by the actual ordinary owner"
    );
    drop(observer);
    assert_review_closing_matches_target(&mut target, &closing, &key).await;
    target.check().await.unwrap();
    setup(&[]).await;
}

async fn unnamed_primary_key_case(new_table: bool) {
    use pbps_db::resolver::capture::ObjectIdentity;

    if new_table {
        setup(&["CREATE SCHEMA pbps_evidence1274"]).await;
    } else {
        setup(&[
            "CREATE SCHEMA pbps_evidence1274",
            "CREATE TABLE pbps_evidence1274.t (n integer NOT NULL, CONSTRAINT unrelated_ck CHECK (n < 100))",
        ])
        .await;
    }
    let table_name: pbps_model::TableName = "pbps_evidence1274.t".parse().unwrap();
    let mut base = Schema::default();
    if !new_table {
        base.tables.insert(table_name.clone(), review_table());
    }
    let mut desired = base.clone();
    let mut keyed = review_table();
    keyed.primary_key = Some(pbps_model::PrimaryKey {
        name: None,
        columns: vec!["n".into()],
    });
    desired.tables.insert(table_name.clone(), keyed);
    let inputs = Inputs::from_pair((base, desired));
    let key = ProjectKey::new(true);
    let mut owned = Some(ObservedContainers::begin());
    let mut target = target().await;
    let mut run = open(Profile::Container, &mut target).await;
    let result = review_plan(&mut target, &mut run, &mut owned, &inputs, &key).await;
    assert!(result.changes.changes.iter().any(|step| {
        if new_table {
            matches!(&step.change, Change::CreateTable { name, .. } if name == &table_name)
        } else {
            matches!(&step.change, Change::SetPrimaryKey { table, to: Some(key), .. }
                if table == &table_name && key.name.is_none())
        }
    }));
    let closing = result.evidence.after().clone();
    let table_id = ObjectIdentity {
        class: "pg_class".into(),
        name: vec![cases::SCHEMA.into(), "t".into()],
        signature: Vec::new(),
    };
    let key_records: Vec<_> = closing
        .prerequisites()
        .iter()
        .filter(|row| {
            row.object.class == "pg_constraint"
                && row.object.signature.get(1) == Some(&table_id)
                && row.ownership == ObjectOwnership::Surface(Surface::Table(table_name.clone()))
        })
        .collect();
    assert_eq!(
        key_records.len(),
        1,
        "the unnamed PK owns one exact table constraint, without sweeping peers"
    );
    if !new_table {
        let unrelated = closing
            .prerequisites()
            .iter()
            .find(|row| {
                row.object.class == "pg_constraint"
                    && row.object.name == ["unrelated_ck"]
                    && row.object.signature.get(1) == Some(&table_id)
            })
            .expect("the unrelated same-parent CHECK is visible in the capture");
        assert_eq!(
            unrelated.ownership,
            ObjectOwnership::Unqualified,
            "a same-parent constraint of another kind gains no PK authority"
        );
    }
    let key_name = &key_records[0].object.name[0];
    let index_id = ObjectIdentity {
        class: "pg_class".into(),
        name: vec![cases::SCHEMA.into(), key_name.clone()],
        signature: Vec::new(),
    };
    assert!(closing.prerequisites().iter().any(|row| {
        row.object == index_id
            && row.ownership == ObjectOwnership::Surface(Surface::Table(table_name.clone()))
    }));
    let mut peer = pbps_db::Conn::connect(
        Driver::Postgres,
        &std::env::var("PBPS_NATIVE_CONNECTION").unwrap(),
    )
    .await
    .unwrap();
    execute_plan(&mut peer, &result.changes).await;
    drop(peer);
    let mut observer = PeerVerifiedConn::connect(
        Driver::Postgres,
        &std::env::var("PBPS_NATIVE_CONNECTION").unwrap(),
    )
    .await
    .unwrap();
    let rows = observer
        .query(
            "SELECT c.conname::text AS key_name, i.relname::text AS index_name, \
                    count(d.objid) AS internal_edges \
             FROM pg_catalog.pg_constraint c \
             JOIN pg_catalog.pg_class t ON t.oid = c.conrelid \
             JOIN pg_catalog.pg_namespace n ON n.oid = t.relnamespace \
             JOIN pg_catalog.pg_class i ON i.oid = c.conindid \
             LEFT JOIN pg_catalog.pg_depend d \
               ON d.classid = 'pg_catalog.pg_class'::regclass AND d.objid = i.oid \
              AND d.refclassid = 'pg_catalog.pg_constraint'::regclass \
              AND d.refobjid = c.oid AND d.deptype = 'i' \
             WHERE n.nspname = 'pbps_evidence1274' AND t.relname = 't' AND c.contype = 'p' \
             GROUP BY c.conname, i.relname",
        )
        .await
        .unwrap();
    assert_eq!(rows.len(), 1, "the actual table has one primary key");
    assert_eq!(
        rows[0].try_get::<&str>("key_name").unwrap(),
        Some(key_name.as_str())
    );
    assert_eq!(
        rows[0].try_get::<&str>("index_name").unwrap(),
        Some(key_name.as_str())
    );
    assert_eq!(rows[0].try_get::<i64>("internal_edges").unwrap(), Some(1));
    drop(observer);
    assert_review_closing_matches_target(&mut target, &closing, &key).await;
    target.check().await.unwrap();
    setup(&[]).await;
}

#[tokio::test]
#[ignore = "requires pinned native PostgreSQL target and owned Docker fixture"]
async fn unnamed_primary_key_on_existing_table_owns_only_its_exact_constraint_and_index() {
    unnamed_primary_key_case(false).await;
}

#[tokio::test]
#[ignore = "requires pinned native PostgreSQL target and owned Docker fixture"]
async fn unnamed_primary_key_on_created_table_owns_only_its_exact_constraint_and_index() {
    unnamed_primary_key_case(true).await;
}

/// A standalone index addition changes the table's catalog flag even though
/// it neither renames nor replaces the relation or its existing column ACL.
#[tokio::test]
#[ignore = "requires pinned native PostgreSQL target and owned Docker fixture"]
async fn adding_index_keeps_existing_table_metadata_and_sets_the_engine_index_flag() {
    setup(&[
        "CREATE SCHEMA pbps_evidence1274",
        "CREATE TABLE pbps_evidence1274.t (n integer NOT NULL)",
        "ALTER TABLE pbps_evidence1274.t OWNER TO pbps_native_alt",
        "GRANT SELECT ON pbps_evidence1274.t TO pg_monitor WITH GRANT OPTION",
        "GRANT UPDATE(n) ON pbps_evidence1274.t TO pg_monitor WITH GRANT OPTION",
    ])
    .await;
    let opening_acl = table_column_grants("t", "n").await;
    let opening_owner = catalog_owner_dependency("table-t").await;
    assert_eq!(opening_owner, ("pbps_native_alt".into(), 1, 1));
    assert_eq!(table_catalog_flags().await, (0, false));
    let mut base = Schema::default();
    base.tables
        .insert("pbps_evidence1274.t".parse().unwrap(), review_table());
    base.roles
        .insert("pbps_native_alt".into(), pbps_model::Role::default());
    let mut desired = base.clone();
    desired
        .tables
        .get_mut(&"pbps_evidence1274.t".parse().unwrap())
        .unwrap()
        .indexes
        .insert(
            "ix".into(),
            pbps_model::Index {
                columns: vec![pbps_model::IndexColumn {
                    key: pbps_model::IndexKey::Column("n".into()),
                    descending: false,
                    opclass: None,
                }],
                include: Vec::new(),
                unique: false,
                filter: None,
                method: Default::default(),
            },
        );
    let inputs = Inputs::from_pair((base, desired));
    let key = ProjectKey::new(true);
    let mut owned = Some(ObservedContainers::begin());
    let mut target = target().await;
    let mut run = open(Profile::Container, &mut target).await;
    let result = review_plan(&mut target, &mut run, &mut owned, &inputs, &key).await;
    assert!(result.changes.changes.iter().any(|step| matches!(
        &step.change,
        Change::AddIndex { table, name, .. }
            if table.to_string() == "pbps_evidence1274.t" && name == "ix"
    )));
    assert!(!result.changes.changes.iter().any(|step| matches!(
        step.change,
        Change::CreateTable { .. } | Change::DropTable { .. } | Change::RenameTable { .. }
    )));
    let closing = result.evidence.after().clone();
    let mut peer = pbps_db::Conn::connect(
        Driver::Postgres,
        &std::env::var("PBPS_NATIVE_CONNECTION").unwrap(),
    )
    .await
    .unwrap();
    execute_plan(&mut peer, &result.changes).await;
    drop(peer);
    assert_eq!(table_column_grants("t", "n").await, opening_acl);
    assert_eq!(catalog_owner_dependency("table-t").await, opening_owner);
    assert_eq!(table_catalog_flags().await, (0, true));
    assert_review_closing_matches_target(&mut target, &closing, &key).await;
    target.check().await.unwrap();
    setup(&[]).await;
}

#[derive(Clone, Copy)]
enum GrantorRoute {
    DirectWithInheritedOwner,
    UniqueInheritedDelegate,
    RevokeBesideOwnerGrant,
}

/// These roles and the schema live only in the runner's disposable target.
/// The actor's prior catalog-read membership is restored after each case.
async fn grantor_route_case(route: GrantorRoute) {
    use pbps_db::resolver::capture::ObjectIdentity;
    use pbps_model::{GrantTarget, Permission, Role};

    const OWNER: &str = "pbps_1274_grant_owner";
    const DELEGATE: &str = "pbps_1274_grant_delegate";
    const OTHER: &str = "pbps_1274_grant_other";
    const ACTOR: &str = "pbps_native_alt";
    const READER: &str = "pg_monitor";
    let inherited = matches!(route, GrantorRoute::UniqueInheritedDelegate);
    let revoke = matches!(route, GrantorRoute::RevokeBesideOwnerGrant);
    let selected_grantor = if inherited { DELEGATE } else { ACTOR };

    setup(&[
        "CREATE SCHEMA pbps_evidence1274",
        "CREATE TABLE pbps_evidence1274.t (n integer NOT NULL)",
    ])
    .await;
    let mut admin = PeerVerifiedConn::connect(
        Driver::Postgres,
        &std::env::var("PBPS_NATIVE_CONNECTION").unwrap(),
    )
    .await
    .unwrap();
    let present = admin
        .query(
            "SELECT count(*)::int8 AS n FROM pg_catalog.pg_roles \
             WHERE rolname IN ('pbps_1274_grant_owner', 'pbps_1274_grant_delegate', \
                               'pbps_1274_grant_other')",
        )
        .await
        .unwrap();
    assert_eq!(present[0].try_get::<i64>("n").unwrap(), Some(0));
    admin
        .query("CREATE ROLE pbps_1274_grant_owner")
        .await
        .unwrap();
    admin
        .query("CREATE ROLE pbps_1274_grant_other")
        .await
        .unwrap();
    if inherited {
        admin
            .query("CREATE ROLE pbps_1274_grant_delegate")
            .await
            .unwrap();
    }
    let settings_membership_sql = concat!(
        "SELECT EXISTS (SELECT 1 FROM pg_catalog.pg_auth_members m ",
        "JOIN pg_catalog.pg_roles granted ON granted.oid = m.roleid ",
        "JOIN pg_catalog.pg_roles member ON member.oid = m.member ",
        "WHERE granted.rolname = 'pg_read_all_settings' ",
        "AND member.rolname = 'pbps_native_alt') AS member"
    );
    let prior = admin.query(settings_membership_sql).await.unwrap();
    assert_eq!(prior.len(), 1);
    let had_settings = prior[0].try_get::<bool>("member").unwrap().unwrap();
    if !had_settings {
        admin
            .query("GRANT pg_read_all_settings TO pbps_native_alt")
            .await
            .unwrap();
    }
    admin
        .query(
            "GRANT USAGE, CREATE ON SCHEMA pbps_evidence1274 \
             TO pbps_native_alt, pbps_1274_grant_owner",
        )
        .await
        .unwrap();
    admin
        .query("ALTER TABLE pbps_evidence1274.t OWNER TO pbps_1274_grant_owner")
        .await
        .unwrap();
    admin
        .query("GRANT SELECT ON pbps_evidence1274.t TO pbps_1274_grant_other")
        .await
        .unwrap();
    if inherited {
        admin
            .query(
                "GRANT SELECT ON pbps_evidence1274.t \
                 TO pbps_1274_grant_delegate WITH GRANT OPTION",
            )
            .await
            .unwrap();
        admin
            .query("GRANT pbps_1274_grant_delegate TO pbps_native_alt")
            .await
            .unwrap();
    } else {
        if matches!(route, GrantorRoute::DirectWithInheritedOwner) {
            admin
                .query("GRANT pbps_1274_grant_owner TO pbps_native_alt")
                .await
                .unwrap();
        }
        admin
            .query(
                "GRANT SELECT ON pbps_evidence1274.t \
                 TO pbps_native_alt WITH GRANT OPTION",
            )
            .await
            .unwrap();
    }
    let inherited_option = admin
        .query(if inherited {
            "SELECT pg_catalog.pg_has_role('pbps_native_alt'::regrole, \
             'pbps_1274_grant_delegate'::regrole, 'USAGE') AS inherited"
        } else {
            "SELECT pg_catalog.pg_has_role('pbps_native_alt'::regrole, \
             'pbps_1274_grant_owner'::regrole, 'USAGE') AS inherited"
        })
        .await
        .unwrap();
    assert_eq!(
        inherited_option[0].try_get::<bool>("inherited").unwrap(),
        Some(!revoke),
        "the actor's measured inherited route is actually usable"
    );
    if revoke {
        // The same grantee/privilege has two original grantors. The actor's
        // later REVOKE cannot erase the owner's independently granted row.
        admin
            .query("GRANT SELECT ON pbps_evidence1274.t TO pg_monitor")
            .await
            .unwrap();
        admin.query("SET ROLE pbps_native_alt").await.unwrap();
        admin
            .query("GRANT SELECT ON pbps_evidence1274.t TO pg_monitor")
            .await
            .unwrap();
        admin.query("RESET ROLE").await.unwrap();
        let opening = admin
            .query(
                "SELECT pg_catalog.pg_get_userbyid(a.grantor)::text AS grantor, \
                        a.is_grantable AS grant_option \
                 FROM pg_catalog.pg_class t \
                 CROSS JOIN LATERAL pg_catalog.aclexplode(t.relacl) a \
                 WHERE t.oid = 'pbps_evidence1274.t'::regclass \
                   AND a.grantee = 'pg_monitor'::regrole \
                   AND a.privilege_type = 'SELECT'",
            )
            .await
            .unwrap();
        let mut grantors = opening
            .iter()
            .map(|row| {
                (
                    row.try_get::<&str>("grantor").unwrap().unwrap().to_owned(),
                    row.try_get::<bool>("grant_option").unwrap().unwrap(),
                )
            })
            .collect::<Vec<_>>();
        grantors.sort();
        let mut expected = vec![(ACTOR.to_owned(), false), (OWNER.to_owned(), false)];
        expected.sort();
        assert_eq!(grantors, expected);
    }
    drop(admin);

    // Admission and its first fresh read bind this same ordinary actor. A
    // separate observer's SET ROLE would not establish the grantor context.
    let mut peer = PeerVerifiedConn::connect(
        Driver::Postgres,
        &std::env::var("PBPS_NATIVE_CONNECTION").unwrap(),
    )
    .await
    .unwrap();
    peer.query("SET ROLE pbps_native_alt").await.unwrap();
    let principal = peer
        .query(
            "SELECT current_user::text AS effective, \
             pg_catalog.current_setting('is_superuser') AS superuser",
        )
        .await
        .unwrap();
    assert_eq!(
        principal[0].try_get::<&str>("effective").unwrap(),
        Some(ACTOR)
    );
    assert_eq!(
        principal[0].try_get::<&str>("superuser").unwrap(),
        Some("off")
    );
    let mut target = NativeTarget::establish(
        peer,
        std::env::var("PBPS_NATIVE_SERVICE_PID")
            .unwrap()
            .parse()
            .unwrap(),
    )
    .await
    .unwrap();
    let table_name: pbps_model::TableName = "pbps_evidence1274.t".parse().unwrap();
    let grant_target: GrantTarget = "pbps_evidence1274.t".parse().unwrap();
    let mut base = Schema::default();
    base.tables.insert(table_name.clone(), review_table());
    for role in [OWNER, OTHER, ACTOR, READER] {
        base.roles.insert(role.into(), Role::default());
    }
    if inherited {
        base.roles.insert(DELEGATE.into(), Role::default());
    }
    let declared_select = BTreeSet::from([Permission::Select]);
    base.roles
        .get_mut(OTHER)
        .unwrap()
        .grants
        .insert(grant_target.clone(), declared_select.clone());
    if revoke {
        base.roles
            .get_mut(READER)
            .unwrap()
            .grants
            .insert(grant_target.clone(), declared_select.clone());
    }
    let mut desired = base.clone();
    if revoke {
        desired.roles.get_mut(READER).unwrap().grants.clear();
    } else {
        desired
            .roles
            .get_mut(READER)
            .unwrap()
            .grants
            .insert(grant_target.clone(), declared_select.clone());
    }
    let inputs = Inputs::from_pair((base, desired));
    let key = ProjectKey::new(true);
    let mut owned = Some(ObservedContainers::begin());
    let mut run = open(Profile::Container, &mut target).await;
    let result = review_plan(&mut target, &mut run, &mut owned, &inputs, &key).await;
    assert!(
        result
            .changes
            .changes
            .iter()
            .any(|step| match &step.change {
                Change::Grant {
                    role,
                    target,
                    permissions,
                } if !revoke => {
                    role == READER && target == &grant_target && permissions == &declared_select
                }
                Change::Revoke {
                    role,
                    target,
                    permissions,
                } if revoke => {
                    role == READER && target == &grant_target && permissions == &declared_select
                }
                Change::CreateTable { .. }
                | Change::DropTable { .. }
                | Change::RenameTable { .. }
                | Change::AddColumn { .. }
                | Change::DropColumn { .. }
                | Change::RenameColumn { .. }
                | Change::AlterColumnType { .. }
                | Change::AlterColumnNullability { .. }
                | Change::AlterColumnDefault { .. }
                | Change::AlterColumnExpression { .. }
                | Change::SetColumnDeprecated { .. }
                | Change::SetPrimaryKey { .. }
                | Change::AddUnique { .. }
                | Change::DropUnique { .. }
                | Change::AddForeignKey { .. }
                | Change::DropForeignKey { .. }
                | Change::AddCheck { .. }
                | Change::DropCheck { .. }
                | Change::AddIndex { .. }
                | Change::DropIndex { .. }
                | Change::InsertRow { .. }
                | Change::UpdateRow { .. }
                | Change::DeleteRow { .. }
                | Change::SetDataMode { .. }
                | Change::CreateModule { .. }
                | Change::AlterModule { .. }
                | Change::DropModule { .. }
                | Change::CreateRole { .. }
                | Change::DropRole { .. }
                | Change::RenameRole { .. }
                | Change::Grant { .. }
                | Change::Revoke { .. }
                | Change::PublicExecution { .. } => false,
            })
    );
    let closing = result.evidence.after().clone();
    let mut writer = pbps_db::Conn::connect(
        Driver::Postgres,
        &std::env::var("PBPS_NATIVE_CONNECTION").unwrap(),
    )
    .await
    .unwrap();
    writer.execute("SET ROLE pbps_native_alt").await.unwrap();
    execute_plan(&mut writer, &result.changes).await;
    drop(writer);
    let mut observer = PeerVerifiedConn::connect(
        Driver::Postgres,
        &std::env::var("PBPS_NATIVE_CONNECTION").unwrap(),
    )
    .await
    .unwrap();
    let acl = observer
        .query(
            "SELECT pg_catalog.pg_get_userbyid(a.grantee)::text AS grantee, \
                    pg_catalog.pg_get_userbyid(a.grantor)::text AS grantor, \
                    a.is_grantable AS grant_option \
             FROM pg_catalog.pg_class t \
             CROSS JOIN LATERAL pg_catalog.aclexplode(t.relacl) a \
             WHERE t.oid = 'pbps_evidence1274.t'::regclass \
               AND a.privilege_type = 'SELECT' \
               AND a.grantee IN ('pg_monitor'::regrole, \
                                 'pbps_1274_grant_other'::regrole)",
        )
        .await
        .unwrap();
    let entries: Vec<_> = acl
        .iter()
        .map(|row| {
            (
                row.try_get::<&str>("grantee").unwrap().unwrap().to_owned(),
                row.try_get::<&str>("grantor").unwrap().unwrap().to_owned(),
                row.try_get::<bool>("grant_option").unwrap().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        entries
            .iter()
            .filter(|entry| entry.0 == OTHER && entry.1 == OWNER && !entry.2)
            .count(),
        1,
        "the unrelated owner's exact ACL row survives the typed step"
    );
    let reader: Vec<_> = entries.iter().filter(|entry| entry.0 == READER).collect();
    if revoke {
        assert_eq!(reader.len(), 1, "only the actor's ACL row is revoked");
        assert_eq!(reader[0].1, OWNER, "the owner's same-grantee row survives");
        assert!(!reader[0].2, "the owner's grant option stays absent");
    } else {
        assert_eq!(reader.len(), 1);
        assert_eq!(reader[0].1, selected_grantor);
        assert!(!reader[0].2, "the typed GRANT adds no grant option");
    }
    drop(observer);
    let observed = assert_review_closing_matches_target(&mut target, &closing, &key).await;
    let table_id = ObjectIdentity {
        class: "pg_class".into(),
        name: vec![cases::SCHEMA.into(), "t".into()],
        signature: Vec::new(),
    };
    let dependencies = |manifest: &pbps_model::resolver::InputManifest| {
        manifest
            .prerequisites()
            .iter()
            .filter(|row| {
                row.object.class == "pg_shdepend" && row.object.signature.first() == Some(&table_id)
            })
            .map(|row| row.object.clone())
            .collect::<BTreeSet<_>>()
    };
    assert!(!dependencies(&closing).is_empty());
    assert_eq!(
        dependencies(&closing),
        dependencies(&observed),
        "the projected owner/ACL shared dependencies equal the fresh post-DDL catalog"
    );
    target.check().await.unwrap();
    drop(target);
    setup(&[]).await;
    let mut admin = PeerVerifiedConn::connect(
        Driver::Postgres,
        &std::env::var("PBPS_NATIVE_CONNECTION").unwrap(),
    )
    .await
    .unwrap();
    if inherited {
        admin
            .query("REVOKE pbps_1274_grant_delegate FROM pbps_native_alt")
            .await
            .unwrap();
    } else if matches!(route, GrantorRoute::DirectWithInheritedOwner) {
        admin
            .query("REVOKE pbps_1274_grant_owner FROM pbps_native_alt")
            .await
            .unwrap();
    }
    if !had_settings {
        admin
            .query("REVOKE pg_read_all_settings FROM pbps_native_alt")
            .await
            .unwrap();
    }
    if inherited {
        admin
            .query("DROP ROLE pbps_1274_grant_delegate")
            .await
            .unwrap();
    }
    admin
        .query("DROP ROLE pbps_1274_grant_other")
        .await
        .unwrap();
    admin
        .query("DROP ROLE pbps_1274_grant_owner")
        .await
        .unwrap();
    let restored = admin.query(settings_membership_sql).await.unwrap();
    assert_eq!(
        restored[0].try_get::<bool>("member").unwrap(),
        Some(had_settings)
    );
    let absent = admin
        .query(
            "SELECT count(*)::int8 AS n FROM pg_catalog.pg_roles \
             WHERE rolname IN ('pbps_1274_grant_owner', 'pbps_1274_grant_delegate', \
                               'pbps_1274_grant_other')",
        )
        .await
        .unwrap();
    assert_eq!(absent[0].try_get::<i64>("n").unwrap(), Some(0));
}

#[tokio::test]
#[ignore = "requires pinned native PostgreSQL target and owned Docker fixture"]
async fn direct_actor_option_beats_inherited_owner_for_retained_table_grant() {
    grantor_route_case(GrantorRoute::DirectWithInheritedOwner).await;
}

#[tokio::test]
#[ignore = "requires pinned native PostgreSQL target and owned Docker fixture"]
async fn unique_inherited_delegate_is_the_retained_table_grantor() {
    grantor_route_case(GrantorRoute::UniqueInheritedDelegate).await;
}

#[tokio::test]
#[ignore = "requires pinned native PostgreSQL target and owned Docker fixture"]
async fn revoke_by_actor_keeps_an_unrelated_owners_grant_row() {
    grantor_route_case(GrantorRoute::RevokeBesideOwnerGrant).await;
}

#[tokio::test]
#[ignore = "requires pinned native PostgreSQL target and owned Docker fixture"]
async fn competing_inherited_grant_options_refuse_before_publishing_evidence() {
    use pbps_model::{GrantTarget, Permission, Role};

    const ACTOR: &str = "pbps_native_alt";
    const FIRST: &str = "pbps_1274_grant_z";
    const SECOND: &str = "pbps_1274_grant_a";
    const READER: &str = "pg_monitor";
    setup(&[
        "CREATE SCHEMA pbps_evidence1274",
        "CREATE TABLE pbps_evidence1274.t (n integer NOT NULL)",
    ])
    .await;
    let mut admin = PeerVerifiedConn::connect(
        Driver::Postgres,
        &std::env::var("PBPS_NATIVE_CONNECTION").unwrap(),
    )
    .await
    .unwrap();
    let present = admin
        .query(
            "SELECT count(*)::int8 AS n FROM pg_catalog.pg_roles \
         WHERE rolname IN ('pbps_1274_grant_z', 'pbps_1274_grant_a')",
        )
        .await
        .unwrap();
    assert_eq!(present[0].try_get::<i64>("n").unwrap(), Some(0));
    // Creation and membership orders disagree with names; neither order is
    // promised as PostgreSQL's chosen grantor (DEC-483).
    admin.query("CREATE ROLE pbps_1274_grant_z").await.unwrap();
    admin.query("CREATE ROLE pbps_1274_grant_a").await.unwrap();
    let settings_membership_sql = concat!(
        "SELECT EXISTS (SELECT 1 FROM pg_catalog.pg_auth_members m ",
        "JOIN pg_catalog.pg_roles granted ON granted.oid = m.roleid ",
        "JOIN pg_catalog.pg_roles member ON member.oid = m.member ",
        "WHERE granted.rolname = 'pg_read_all_settings' ",
        "AND member.rolname = 'pbps_native_alt') AS member"
    );
    let prior = admin.query(settings_membership_sql).await.unwrap();
    let had_settings = prior[0].try_get::<bool>("member").unwrap().unwrap();
    if !had_settings {
        admin
            .query("GRANT pg_read_all_settings TO pbps_native_alt")
            .await
            .unwrap();
    }
    admin
        .query("GRANT USAGE, CREATE ON SCHEMA pbps_evidence1274 TO pbps_native_alt")
        .await
        .unwrap();
    admin
        .query(
            "GRANT SELECT ON pbps_evidence1274.t \
         TO pbps_1274_grant_z, pbps_1274_grant_a WITH GRANT OPTION",
        )
        .await
        .unwrap();
    admin
        .query("GRANT pbps_1274_grant_a TO pbps_native_alt")
        .await
        .unwrap();
    admin
        .query("GRANT pbps_1274_grant_z TO pbps_native_alt")
        .await
        .unwrap();
    let options = admin
        .query(
            "SELECT count(*)::int8 AS n FROM pg_catalog.pg_class t \
         CROSS JOIN LATERAL pg_catalog.aclexplode(t.relacl) a \
         WHERE t.oid = 'pbps_evidence1274.t'::regclass \
           AND a.grantee IN ('pbps_1274_grant_z'::regrole, \
                             'pbps_1274_grant_a'::regrole) \
           AND a.privilege_type = 'SELECT' AND a.is_grantable",
        )
        .await
        .unwrap();
    assert_eq!(options[0].try_get::<i64>("n").unwrap(), Some(2));
    for role in [FIRST, SECOND] {
        let inherited = admin
            .query(&format!(
                "SELECT pg_catalog.pg_has_role('pbps_native_alt'::regrole, \
             '{role}'::regrole, 'USAGE') AS inherited"
            ))
            .await
            .unwrap();
        assert_eq!(
            inherited[0].try_get::<bool>("inherited").unwrap(),
            Some(true)
        );
    }
    drop(admin);

    let mut peer = PeerVerifiedConn::connect(
        Driver::Postgres,
        &std::env::var("PBPS_NATIVE_CONNECTION").unwrap(),
    )
    .await
    .unwrap();
    peer.query("SET ROLE pbps_native_alt").await.unwrap();
    let principal = peer
        .query(
            "SELECT current_user::text AS effective, \
         pg_catalog.current_setting('is_superuser') AS superuser",
        )
        .await
        .unwrap();
    assert_eq!(
        principal[0].try_get::<&str>("effective").unwrap(),
        Some(ACTOR)
    );
    assert_eq!(
        principal[0].try_get::<&str>("superuser").unwrap(),
        Some("off")
    );
    let mut target = NativeTarget::establish(
        peer,
        std::env::var("PBPS_NATIVE_SERVICE_PID")
            .unwrap()
            .parse()
            .unwrap(),
    )
    .await
    .unwrap();
    let mut base = Schema::default();
    base.tables
        .insert("pbps_evidence1274.t".parse().unwrap(), review_table());
    for role in [ACTOR, FIRST, SECOND, READER] {
        base.roles.insert(role.into(), Role::default());
    }
    let mut desired = base.clone();
    let grant_target: GrantTarget = "pbps_evidence1274.t".parse().unwrap();
    desired
        .roles
        .get_mut(READER)
        .unwrap()
        .grants
        .insert(grant_target, BTreeSet::from([Permission::Select]));
    let inputs = Inputs::from_pair((base, desired));
    let key = ProjectKey::new(true);
    let mut owned = Some(ObservedContainers::begin());
    let mut run = open(Profile::Container, &mut target).await;
    let result = run
        .plan_resolved(
            &mut target,
            &inputs.binding(),
            inputs.base(),
            inputs.desired(),
            &inputs.hints,
            &[],
            &key.project,
            Some(ENVIRONMENT),
        )
        .await;
    close(&mut run, &mut owned).await;
    drop(target);
    let refusal = match result {
        Err(Error::Binding(message)) => message,
        Err(error) => format!("unexpected refusal: {error}"),
        Ok(_) => "unexpected producer success".into(),
    };
    let mut observer = PeerVerifiedConn::connect(
        Driver::Postgres,
        &std::env::var("PBPS_NATIVE_CONNECTION").unwrap(),
    )
    .await
    .unwrap();
    let reader_acl = observer
        .query(
            "SELECT count(*)::int8 AS n FROM pg_catalog.pg_class t \
         CROSS JOIN LATERAL pg_catalog.aclexplode(t.relacl) a \
         WHERE t.oid = 'pbps_evidence1274.t'::regclass \
           AND a.grantee = 'pg_monitor'::regrole \
           AND a.privilege_type = 'SELECT'",
        )
        .await
        .unwrap();
    let reader_rows = reader_acl[0].try_get::<i64>("n").unwrap();
    drop(observer);
    setup(&[]).await;
    let mut admin = PeerVerifiedConn::connect(
        Driver::Postgres,
        &std::env::var("PBPS_NATIVE_CONNECTION").unwrap(),
    )
    .await
    .unwrap();
    admin
        .query("REVOKE pbps_1274_grant_a FROM pbps_native_alt")
        .await
        .unwrap();
    admin
        .query("REVOKE pbps_1274_grant_z FROM pbps_native_alt")
        .await
        .unwrap();
    if !had_settings {
        admin
            .query("REVOKE pg_read_all_settings FROM pbps_native_alt")
            .await
            .unwrap();
    }
    admin.query("DROP ROLE pbps_1274_grant_a").await.unwrap();
    admin.query("DROP ROLE pbps_1274_grant_z").await.unwrap();
    let restored = admin.query(settings_membership_sql).await.unwrap();
    assert_eq!(
        restored[0].try_get::<bool>("member").unwrap(),
        Some(had_settings)
    );
    let absent = admin
        .query(
            "SELECT count(*)::int8 AS n FROM pg_catalog.pg_roles \
         WHERE rolname IN ('pbps_1274_grant_z', 'pbps_1274_grant_a')",
        )
        .await
        .unwrap();
    assert_eq!(absent[0].try_get::<i64>("n").unwrap(), Some(0));
    assert_eq!(
        reader_rows,
        Some(0),
        "refusal left the target ACL unchanged"
    );
    assert!(
        refusal.contains("final compiled catalog manifest"),
        "two inherited candidates must refuse before evidence: {refusal}"
    );
}

#[derive(Clone, Copy)]
enum SchemaGrantorRoute {
    DirectOverInheritedOwner,
    WholeStatementInheritedDelegate,
}

/// One typed GRANT is one PostgreSQL grantor choice across its permission mask.
async fn schema_grantor_case(route: SchemaGrantorRoute) {
    use pbps_model::{GrantTarget, Permission, Role};

    const OWNER: &str = "pbps_1274_schema_owner";
    const DELEGATE: &str = "pbps_1274_schema_delegate";
    const ACTOR: &str = "pbps_native_alt";
    const READER: &str = "pg_monitor";
    let combined = matches!(route, SchemaGrantorRoute::WholeStatementInheritedDelegate);
    setup(&[
        "CREATE SCHEMA pbps_evidence1274",
        "CREATE TABLE pbps_evidence1274.t (n integer NOT NULL)",
    ])
    .await;
    let mut admin = PeerVerifiedConn::connect(
        Driver::Postgres,
        &std::env::var("PBPS_NATIVE_CONNECTION").unwrap(),
    )
    .await
    .unwrap();
    let present = admin
        .query(
            "SELECT count(*)::int8 AS n FROM pg_catalog.pg_roles \
         WHERE rolname IN ('pbps_1274_schema_owner', 'pbps_1274_schema_delegate')",
        )
        .await
        .unwrap();
    assert_eq!(present[0].try_get::<i64>("n").unwrap(), Some(0));
    admin
        .query("CREATE ROLE pbps_1274_schema_owner")
        .await
        .unwrap();
    if combined {
        admin
            .query("CREATE ROLE pbps_1274_schema_delegate")
            .await
            .unwrap();
    }
    let settings_membership_sql = concat!(
        "SELECT EXISTS (SELECT 1 FROM pg_catalog.pg_auth_members m ",
        "JOIN pg_catalog.pg_roles granted ON granted.oid = m.roleid ",
        "JOIN pg_catalog.pg_roles member ON member.oid = m.member ",
        "WHERE granted.rolname = 'pg_read_all_settings' ",
        "AND member.rolname = 'pbps_native_alt') AS member"
    );
    let prior = admin.query(settings_membership_sql).await.unwrap();
    let had_settings = prior[0].try_get::<bool>("member").unwrap().unwrap();
    if !had_settings {
        admin
            .query("GRANT pg_read_all_settings TO pbps_native_alt")
            .await
            .unwrap();
    }
    admin
        .query("ALTER SCHEMA pbps_evidence1274 OWNER TO pbps_1274_schema_owner")
        .await
        .unwrap();
    admin
        .query("GRANT USAGE ON SCHEMA pbps_evidence1274 TO pbps_native_alt WITH GRANT OPTION")
        .await
        .unwrap();
    if combined {
        admin
            .query(
                "GRANT USAGE, CREATE ON SCHEMA pbps_evidence1274 \
             TO pbps_1274_schema_delegate WITH GRANT OPTION",
            )
            .await
            .unwrap();
        admin
            .query("GRANT pbps_1274_schema_delegate TO pbps_native_alt")
            .await
            .unwrap();
    } else {
        admin
            .query("GRANT pbps_1274_schema_owner TO pbps_native_alt")
            .await
            .unwrap();
    }
    let inherited_role = if combined { DELEGATE } else { OWNER };
    let inherited = admin
        .query(&format!(
            "SELECT pg_catalog.pg_has_role('pbps_native_alt'::regrole, \
         '{inherited_role}'::regrole, 'USAGE') AS inherited"
        ))
        .await
        .unwrap();
    assert_eq!(
        inherited[0].try_get::<bool>("inherited").unwrap(),
        Some(true)
    );
    drop(admin);

    let mut peer = PeerVerifiedConn::connect(
        Driver::Postgres,
        &std::env::var("PBPS_NATIVE_CONNECTION").unwrap(),
    )
    .await
    .unwrap();
    peer.query("SET ROLE pbps_native_alt").await.unwrap();
    let principal = peer
        .query(
            "SELECT current_user::text AS effective, \
         pg_catalog.current_setting('is_superuser') AS superuser",
        )
        .await
        .unwrap();
    assert_eq!(
        principal[0].try_get::<&str>("effective").unwrap(),
        Some(ACTOR)
    );
    assert_eq!(
        principal[0].try_get::<&str>("superuser").unwrap(),
        Some("off")
    );
    let mut target = NativeTarget::establish(
        peer,
        std::env::var("PBPS_NATIVE_SERVICE_PID")
            .unwrap()
            .parse()
            .unwrap(),
    )
    .await
    .unwrap();
    let mut base = Schema::default();
    base.tables
        .insert("pbps_evidence1274.t".parse().unwrap(), review_table());
    for role in [OWNER, ACTOR, READER] {
        base.roles.insert(role.into(), Role::default());
    }
    if combined {
        base.roles.insert(DELEGATE.into(), Role::default());
    }
    let mut desired = base.clone();
    let permissions = if combined {
        BTreeSet::from([Permission::Usage, Permission::Create])
    } else {
        BTreeSet::from([Permission::Usage])
    };
    let schema_target = GrantTarget::Schema(cases::SCHEMA.into());
    desired
        .roles
        .get_mut(READER)
        .unwrap()
        .grants
        .insert(schema_target.clone(), permissions.clone());
    let inputs = Inputs::from_pair((base, desired));
    let key = ProjectKey::new(true);
    let mut owned = Some(ObservedContainers::begin());
    let mut run = open(Profile::Container, &mut target).await;
    let result = review_plan(&mut target, &mut run, &mut owned, &inputs, &key).await;
    assert!(result.changes.changes.iter().any(|step| matches!(
        &step.change,
        Change::Grant { role, target, permissions: granted }
            if role == READER && target == &schema_target && granted == &permissions
    )));
    let closing = result.evidence.after().clone();
    let mut writer = pbps_db::Conn::connect(
        Driver::Postgres,
        &std::env::var("PBPS_NATIVE_CONNECTION").unwrap(),
    )
    .await
    .unwrap();
    writer.execute("SET ROLE pbps_native_alt").await.unwrap();
    execute_plan(&mut writer, &result.changes).await;
    drop(writer);
    let mut observer = PeerVerifiedConn::connect(
        Driver::Postgres,
        &std::env::var("PBPS_NATIVE_CONNECTION").unwrap(),
    )
    .await
    .unwrap();
    let acl = observer
        .query(
            "SELECT a.privilege_type::text AS privilege, \
                pg_catalog.pg_get_userbyid(a.grantor)::text AS grantor, \
                a.is_grantable AS grant_option \
         FROM pg_catalog.pg_namespace n \
         CROSS JOIN LATERAL pg_catalog.aclexplode(n.nspacl) a \
         WHERE n.nspname = 'pbps_evidence1274' \
           AND a.grantee = 'pg_monitor'::regrole \
           AND a.privilege_type IN ('USAGE', 'CREATE')",
        )
        .await
        .unwrap();
    let mut actual: Vec<_> = acl
        .iter()
        .map(|row| {
            (
                row.try_get::<&str>("privilege")
                    .unwrap()
                    .unwrap()
                    .to_owned(),
                row.try_get::<&str>("grantor").unwrap().unwrap().to_owned(),
                row.try_get::<bool>("grant_option").unwrap().unwrap(),
            )
        })
        .collect();
    actual.sort();
    let selected = if combined { DELEGATE } else { ACTOR };
    let mut expected = if combined {
        vec![
            ("CREATE".to_owned(), selected.to_owned(), false),
            ("USAGE".to_owned(), selected.to_owned(), false),
        ]
    } else {
        vec![("USAGE".to_owned(), selected.to_owned(), false)]
    };
    expected.sort();
    assert_eq!(
        actual, expected,
        "one typed statement has one selected grantor"
    );
    drop(observer);
    assert_review_closing_matches_target(&mut target, &closing, &key).await;
    // Re-read as the original deployer after DDL. A catalog-only oracle would
    // miss an incorrect projection of the grouped schema grant's authorization.
    let schemas = vec![cases::SCHEMA.to_owned()];
    let (_, fresh_authorization) = target
        .scope_facts(&schemas, &[], &schemas, &[])
        .await
        .unwrap();
    let selected =
        pbps_db::fingerprint::EnvironmentFingerprintKey::from_file(&key.root.join("key")).unwrap();
    let fresh = fresh_authorization
        .persisted(&selected, &ChangeSet::default())
        .unwrap();
    assert_eq!(
        fresh.before,
        result.evidence.authorization().after,
        "the sealed closing authorization equals the same actor's fresh post-DDL context"
    );
    target.check().await.unwrap();
    drop(target);
    setup(&[]).await;
    let mut admin = PeerVerifiedConn::connect(
        Driver::Postgres,
        &std::env::var("PBPS_NATIVE_CONNECTION").unwrap(),
    )
    .await
    .unwrap();
    admin
        .query(&format!("REVOKE {inherited_role} FROM pbps_native_alt"))
        .await
        .unwrap();
    if !had_settings {
        admin
            .query("REVOKE pg_read_all_settings FROM pbps_native_alt")
            .await
            .unwrap();
    }
    if combined {
        admin
            .query("DROP ROLE pbps_1274_schema_delegate")
            .await
            .unwrap();
    }
    admin
        .query("DROP ROLE pbps_1274_schema_owner")
        .await
        .unwrap();
    let restored = admin.query(settings_membership_sql).await.unwrap();
    assert_eq!(
        restored[0].try_get::<bool>("member").unwrap(),
        Some(had_settings)
    );
    let absent = admin
        .query(
            "SELECT count(*)::int8 AS n FROM pg_catalog.pg_roles \
         WHERE rolname IN ('pbps_1274_schema_owner', 'pbps_1274_schema_delegate')",
        )
        .await
        .unwrap();
    assert_eq!(absent[0].try_get::<i64>("n").unwrap(), Some(0));
}

#[tokio::test]
#[ignore = "requires pinned native PostgreSQL target and owned Docker fixture"]
async fn direct_schema_option_beats_inherited_owner_for_the_typed_grant() {
    schema_grantor_case(SchemaGrantorRoute::DirectOverInheritedOwner).await;
}

#[tokio::test]
#[ignore = "requires pinned native PostgreSQL target and owned Docker fixture"]
async fn combined_schema_grant_uses_one_inherited_grantor_for_its_whole_mask() {
    schema_grantor_case(SchemaGrantorRoute::WholeStatementInheritedDelegate).await;
}

#[derive(Clone, Copy)]
enum GenerationCase {
    SetExpression,
    AddStored,
}

async fn generation_producer_case(case: GenerationCase) {
    use pbps_db::fingerprint::EnvironmentFingerprintKey;
    use pbps_model::{Column, Generated};

    let set_expression = matches!(case, GenerationCase::SetExpression);
    let expected_major = if set_expression { 18 } else { 16 };
    let case_name = if set_expression {
        "pg18-set-expression"
    } else {
        "pg16-add-stored"
    };
    let table = pbps_model::TableName::new(cases::SCHEMA, "t");
    let base = generation_schema(&table);
    let mut desired = base.clone();
    let columns = &mut desired.tables.get_mut(&table).unwrap().columns;
    if set_expression {
        columns["g"].generated.as_mut().unwrap().expression = "a * 3".into();
    } else {
        let mut h = Column::new("integer".parse().unwrap());
        h.generated = Some(Generated {
            expression: "a + 4".into(),
            stored: true,
        });
        columns.insert("h".into(), h);
    }
    let inputs = Inputs::from_pair((base, desired));
    let key = ProjectKey::new(true);
    setup(&[
        "CREATE SCHEMA pbps_evidence1274",
        "GRANT USAGE, CREATE ON SCHEMA pbps_evidence1274 TO pbps_native_alt",
        "CREATE TABLE pbps_evidence1274.t (a integer, \
         g integer GENERATED ALWAYS AS (a * 2 + 1) STORED, d integer DEFAULT 7)",
        "ALTER TABLE pbps_evidence1274.t OWNER TO pbps_native_alt",
        "INSERT INTO pbps_evidence1274.t (a) VALUES (5)",
    ])
    .await;
    const MEMBERSHIP: &str = "SELECT EXISTS (SELECT 1 FROM pg_catalog.pg_auth_members m \
        JOIN pg_catalog.pg_roles granted ON granted.oid = m.roleid \
        JOIN pg_catalog.pg_roles member ON member.oid = m.member \
        WHERE granted.rolname = 'pg_read_all_settings' \
          AND member.rolname = 'pbps_native_alt') AS member";
    const PRINCIPAL: &str = "SELECT current_user::text AS effective, \
        session_user::text AS login, pg_catalog.current_setting('is_superuser') AS superuser, \
        r.rolsuper, r.rolcreatedb, r.rolcreaterole \
        FROM pg_catalog.pg_roles r WHERE r.rolname = current_user";
    // Raw addresses are observed only to distinguish replacement from retention;
    // the saved evidence must use logical objects and recorded UIDs instead.
    const INVENTORY: &str = "SELECT json_agg(row_to_json(x) ORDER BY x.name)::text AS inventory \
        FROM (SELECT c.oid::int8 AS table_oid, a.attname::text AS name, \
        a.attnum::int8 AS column_number, a.attgenerated::text AS generated, \
        ad.oid::int8 AS attrdef_oid, \
        (SELECT count(*) FROM pg_catalog.pg_depend p \
         WHERE p.classid = 'pg_catalog.pg_attrdef'::regclass AND p.objid = ad.oid \
           AND p.refclassid = 'pg_catalog.pg_class'::regclass AND p.refobjid = c.oid \
           AND p.refobjsubid = a.attnum AND p.deptype = 'i') AS internal_owner, \
        (SELECT count(*) FROM pg_catalog.pg_depend p \
         JOIN pg_catalog.pg_attribute input ON input.attrelid = c.oid \
          AND input.attname = 'a' AND input.attnum = p.refobjsubid \
         WHERE p.classid = 'pg_catalog.pg_attrdef'::regclass AND p.objid = ad.oid \
           AND p.refclassid = 'pg_catalog.pg_class'::regclass AND p.refobjid = c.oid \
           AND p.deptype = 'n') AS input_reference \
        FROM pg_catalog.pg_class c JOIN pg_catalog.pg_attribute a ON a.attrelid = c.oid \
        LEFT JOIN pg_catalog.pg_attrdef ad ON ad.adrelid = c.oid AND ad.adnum = a.attnum \
        WHERE c.oid = 'pbps_evidence1274.t'::regclass \
          AND a.attnum > 0 AND NOT a.attisdropped) x";
    let mut peer = PeerVerifiedConn::connect(
        Driver::Postgres,
        &std::env::var("PBPS_NATIVE_CONNECTION").unwrap(),
    )
    .await
    .unwrap();
    let membership = peer.query(MEMBERSHIP).await.unwrap();
    let had_settings = membership[0].try_get::<bool>("member").unwrap().unwrap();
    if !had_settings {
        peer.query("GRANT pg_read_all_settings TO pbps_native_alt")
            .await
            .unwrap();
    }
    peer.query("SET ROLE pbps_native_alt").await.unwrap();
    let target_principal = peer.query(PRINCIPAL).await.unwrap();
    let before_catalog = peer.query(INVENTORY).await.unwrap();
    let before_catalog = before_catalog[0]
        .try_get::<&str>("inventory")
        .unwrap()
        .unwrap()
        .to_owned();
    let before_values = peer
        .query("SELECT json_build_array(a, g, d)::text AS values FROM pbps_evidence1274.t")
        .await
        .unwrap();
    let before_values = before_values[0]
        .try_get::<&str>("values")
        .unwrap()
        .unwrap()
        .to_owned();
    let mut target = NativeTarget::establish(
        peer,
        std::env::var("PBPS_NATIVE_SERVICE_PID")
            .unwrap()
            .parse()
            .unwrap(),
    )
    .await
    .unwrap();
    let mut owned = Some(ObservedContainers::begin());
    let mut run = open(Profile::Container, &mut target).await;
    let result = run
        .plan_resolved(
            &mut target,
            &inputs.binding(),
            inputs.base(),
            inputs.desired(),
            &inputs.hints,
            &[],
            &key.project,
            Some(ENVIRONMENT),
        )
        .await;
    // Read the retained qualification and that very scratch connection; the
    // fixture's major selector is not evidence of either running engine.
    let observations = async {
        run.check(&mut target).await?;
        let sealed = run.scope.as_ref().ok_or(Error::Cancelled)?;
        let versions = [
            sealed
                .target
                .catalog
                .observations
                .get("server_version_num")
                .cloned()
                .ok_or_else(|| Error::Scope("target version was not reported".into()))?,
            sealed
                .scratch_facts
                .catalog
                .observations
                .get("server_version_num")
                .cloned()
                .ok_or_else(|| Error::Scope("scratch version was not reported".into()))?,
        ];
        let deployer = sealed
            .map
            .deployer(&sealed.authorization_context)
            .map_err(|reason| Error::Scope(reason.into()))?;
        let scratch_login = run.names.login().to_owned();
        let scratch = run.scratch.as_mut().ok_or(Error::Cancelled)?;
        if scratch.connection().id() != sealed.scratch_connection {
            return Err(Error::Scope(
                "generation observation changed its connection".into(),
            ));
        }
        let principal = scratch
            .connection_mut()
            .query(PRINCIPAL)
            .await
            .map_err(|error| Error::Scope(error.to_string()))?;
        Ok((versions, deployer, scratch_login, principal))
    }
    .await;
    close(&mut run, &mut owned).await;
    // Even a producer/reader refusal must restore the temporary target grant
    // before the intended property failure is reported.
    let outcome = async {
        let result = result.map_err(|error| error.to_string())?;
        let observations = observations.map_err(|error| error.to_string())?;
        result
            .evidence
            .validate(&result.changes)
            .map_err(|error| error.to_string())?;
        pbps_pg::resolver::validate_evidence(&result.evidence)
            .map_err(|error| error.to_string())?;
        let artifact = SavedPlan::new(
            PlanOrigin::Database,
            "postgres",
            "1274 native generated column",
            PlanBaseline {
                description: "independent generated-column target".into(),
                checksum: "00".repeat(32),
                database_collation: None,
            },
            result.changes,
            inputs.desired_ids.clone(),
        )
        .with_resolution(result.evidence)
        .map_err(|error| error.to_string())?;
        let serialized = serde_json::to_string(&artifact).map_err(|error| error.to_string())?;
        let restored: SavedPlan =
            serde_json::from_str(&serialized).map_err(|error| error.to_string())?;
        restored
            .validate_analysis()
            .map_err(|error| error.to_string())?;
        let PlanAnalysis::Resolved(evidence) = &restored.analysis else {
            return Err("roundtrip lost the generated-column evidence".into());
        };
        pbps_pg::resolver::validate_evidence(evidence).map_err(|error| error.to_string())?;
        let mut writer = pbps_db::Conn::connect(
            Driver::Postgres,
            &std::env::var("PBPS_NATIVE_CONNECTION").unwrap(),
        )
        .await
        .map_err(|error| error.to_string())?;
        writer
            .execute("SET ROLE pbps_native_alt")
            .await
            .map_err(|error| error.to_string())?;
        let writer_principal = writer
            .query(PRINCIPAL)
            .await
            .map_err(|error| error.to_string())?;
        execute_plan(&mut writer, &restored.changes).await;
        let after_catalog = writer
            .query(INVENTORY)
            .await
            .map_err(|error| error.to_string())?;
        let values_sql = if set_expression {
            "SELECT json_build_array(a, g, d)::text AS values FROM pbps_evidence1274.t"
        } else {
            "SELECT json_build_array(a, g, d, h)::text AS values FROM pbps_evidence1274.t"
        };
        let after_values = writer
            .query(values_sql)
            .await
            .map_err(|error| error.to_string())?;
        drop(writer);
        let selected = EnvironmentFingerprintKey::from_file(&key.root.join("key"))
            .map_err(|error| error.to_string())?;
        // Reread every projected prerequisite, including inputs no longer reached
        // by the closing bindings. Address rows come from rooted subjects because
        // the capture index excludes these address-only catalog classes.
        let mut closing_scope = catalog_scope(evidence.after().scope());
        closing_scope.retained.extend(
            evidence
                .after()
                .prerequisites()
                .iter()
                .filter(|record| {
                    !matches!(
                        record.object.class.as_str(),
                        "pg_depend" | "pg_shdepend" | "pg_init_privs"
                    )
                })
                .map(|record| record.object.clone()),
        );
        let (_, observed) = target
            .capture_postgres_sealed(&closing_scope, &Default::default(), &selected)
            .await
            .map_err(|error| error.to_string())?;
        target.check().await.map_err(|error| error.to_string())?;
        Ok::<_, String>((
            artifact,
            restored,
            observations,
            writer_principal,
            after_catalog,
            after_values,
            observed,
        ))
    }
    .await;
    drop(run);
    drop(target);
    let mut admin = PeerVerifiedConn::connect(
        Driver::Postgres,
        &std::env::var("PBPS_NATIVE_CONNECTION").unwrap(),
    )
    .await
    .unwrap();
    if !had_settings {
        admin
            .query("REVOKE pg_read_all_settings FROM pbps_native_alt")
            .await
            .unwrap();
    }
    let restored_membership = admin.query(MEMBERSHIP).await.unwrap();
    drop(admin);
    setup(&[]).await;

    let (
        artifact,
        restored,
        (versions, deployer, scratch_login, scratch_principal),
        writer_principal,
        after_catalog,
        after_values,
        observed,
    ) = outcome
        .expect("the generated-column producer, artifact reader and typed apply must succeed");
    assert_eq!(
        restored_membership[0].try_get::<bool>("member").unwrap(),
        Some(had_settings)
    );
    for (side, version) in ["target", "scratch"].into_iter().zip(versions) {
        let numeric = version
            .value()
            .expect("qualification reported an actual numeric version")
            .parse::<i64>()
            .unwrap();
        assert_eq!(numeric / 10_000, expected_major, "{side} actual major");
        eprintln!(
            "PBPS1274_GENERATION {}",
            serde_json::json!({
                "case": case_name,
                "phase": "qualified",
                "side": side,
                "server_version_num": numeric,
                "observed_major": numeric / 10_000,
            })
        );
    }
    for (side, rows, effective, login) in [
        ("target", &target_principal, "pbps_native_alt", "postgres"),
        (
            "scratch",
            &scratch_principal,
            deployer.as_deref().unwrap(),
            scratch_login.as_str(),
        ),
        ("writer", &writer_principal, "pbps_native_alt", "postgres"),
    ] {
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].try_get::<&str>("effective").unwrap(),
            Some(effective)
        );
        assert_eq!(rows[0].try_get::<&str>("login").unwrap(), Some(login));
        assert_eq!(rows[0].try_get::<&str>("superuser").unwrap(), Some("off"));
        for flag in ["rolsuper", "rolcreatedb", "rolcreaterole"] {
            assert_eq!(
                rows[0].try_get::<bool>(flag).unwrap(),
                Some(false),
                "{side} {flag}"
            );
        }
        eprintln!(
            "generation_principal side={side} effective={effective} login={login} ordinary=true"
        );
    }
    let before: Value = serde_json::from_str(&before_catalog).unwrap();
    let after: Value = serde_json::from_str(
        after_catalog[0]
            .try_get::<&str>("inventory")
            .unwrap()
            .unwrap(),
    )
    .unwrap();
    let before_values: Value = serde_json::from_str(&before_values).unwrap();
    let after_values: Value =
        serde_json::from_str(after_values[0].try_get::<&str>("values").unwrap().unwrap()).unwrap();
    eprintln!(
        "PBPS1274_GENERATION {}",
        serde_json::json!({
            "case": case_name,
            "phase": "observed",
            "before_catalog": before,
            "after_catalog": after,
            "before_values": {"a": before_values[0], "g": before_values[1], "d": before_values[2]},
            "after_values": after_values,
            "after_value_columns": if set_expression { vec!["a", "g", "d"] } else { vec!["a", "g", "d", "h"] },
        })
    );
    assert_eq!(restored.checksum(), artifact.checksum());
    let PlanAnalysis::Resolved(evidence) = &restored.analysis else {
        unreachable!()
    };
    assert_eq!(
        restored.ids.table_uid(&table),
        inputs.base_ids.table_uid(&table)
    );
    for name in ["a", "g", "d"] {
        assert_eq!(
            restored.ids.column_uid(&table.column(name)),
            inputs.base_ids.column_uid(&table.column(name))
        );
    }
    assert_eq!(
        restored.changes.changes.len(),
        1,
        "one typed generated-column change"
    );
    let changed = &restored.changes.changes[0].change;
    if set_expression {
        assert!(
            matches!(changed, Change::AlterColumnExpression { uid, column, from, to }
            if Some(uid) == inputs.base_ids.column_uid(&table.column("g"))
                && column == &table.column("g") && from == "a * 2 + 1" && to == "a * 3")
        );
    } else {
        assert!(
            matches!(changed, Change::AddColumn { uid, table: selected, name, column }
            if Some(uid) == restored.ids.column_uid(&table.column("h"))
                && selected == &table && name == "h"
                && column.as_ref() == &inputs.desired.tables[&table].columns["h"])
        );
    }
    let required = if set_expression {
        vec!["g", "d"]
    } else {
        vec!["g", "d", "h"]
    };
    assert_eq!(evidence.surfaces().len(), required.len());
    for name in required {
        let [column, attrdef, owner, reference] = generation_objects(&table, name);
        let surface = Surface::Default(table.column(name));
        let resolutions: Vec<_> = evidence
            .surfaces()
            .iter()
            .filter(|row| row.surface == surface)
            .collect();
        assert_eq!(resolutions.len(), 1, "one required {name} attrdef surface");
        assert_eq!(resolutions[0].current.is_some(), name != "h");
        if let Some(current) = &resolutions[0].current {
            assert_eq!(current.object, attrdef);
        }
        assert_eq!(resolutions[0].desired.as_ref().unwrap().object, attrdef);
        for (opening, manifest) in [(true, evidence.before()), (false, evidence.after())] {
            let objects = if name == "d" {
                vec![attrdef.clone()]
            } else {
                vec![attrdef.clone(), owner.clone(), reference.clone()]
            };
            for object in objects {
                let records: Vec<_> = manifest
                    .prerequisites()
                    .iter()
                    .filter(|row| row.object == object)
                    .collect();
                if opening && name == "h" {
                    assert!(records.is_empty());
                } else {
                    assert_eq!(
                        records.len(),
                        1,
                        "exact child inventory for {name}: {object:?}"
                    );
                    assert_eq!(
                        records[0].ownership,
                        ObjectOwnership::Surface(surface.clone())
                    );
                }
            }
            let columns: Vec<_> = manifest
                .prerequisites()
                .iter()
                .filter(|row| row.object == column)
                .collect();
            assert_eq!(columns.len(), usize::from(!opening || name != "h"));
            if let Some(record) = columns.first() {
                assert_eq!(
                    record.ownership,
                    ObjectOwnership::Surface(Surface::Column(table.column(name)))
                );
            }
        }
    }
    let expected = evidence.after().prerequisites();
    // A net count cannot distinguish missing identities from new arrivals.
    // Diagnose only held snapshots so a mismatch cannot change the observation.
    let expected_records: BTreeMap<_, _> = expected
        .iter()
        .map(|record| (record.object.clone(), record))
        .collect();
    let observed_records: BTreeMap<_, _> = observed
        .prerequisites()
        .iter()
        .map(|record| (record.object.clone(), record))
        .collect();
    let missing: Vec<_> = expected_records
        .keys()
        .filter(|object| !observed_records.contains_key(*object))
        .collect();
    let unexpected: Vec<_> = observed_records
        .keys()
        .filter(|object| !expected_records.contains_key(*object))
        .collect();
    let common: Vec<_> = expected_records
        .keys()
        .filter(|object| observed_records.contains_key(*object))
        .collect();
    let properties_mismatches: Vec<_> = common
        .iter()
        .copied()
        .filter(|object| {
            expected_records[*object].properties != observed_records[*object].properties
        })
        .collect();
    let bindings_mismatches: Vec<_> = common
        .iter()
        .copied()
        .filter(|object| expected_records[*object].bindings != observed_records[*object].bindings)
        .collect();
    if !missing.is_empty()
        || !unexpected.is_empty()
        || !properties_mismatches.is_empty()
        || !bindings_mismatches.is_empty()
    {
        let describe = |object: &pbps_model::resolver::ObjectIdentity| {
            let opening = evidence
                .before()
                .prerequisites()
                .iter()
                .find(|record| &record.object == object);
            serde_json::json!({
                "object": object,
                "expected_ownership": expected_records.get(object).map(|record| &record.ownership),
                "observed_ownership": observed_records.get(object).map(|record| &record.ownership),
                "opening_present": opening.is_some(),
                "opening_ownership": opening.map(|record| &record.ownership),
                "retained_root_present": {
                    "opening": evidence.before().scope().retained.contains(object),
                    "desired": evidence.after().scope().retained.contains(object),
                    "fresh": observed.scope().retained.contains(object),
                },
                "candidate_membership_present": {
                    "opening": evidence.before().membership().iter().any(|row| row.members.contains(object)),
                    "desired": evidence.after().membership().iter().any(|row| row.members.contains(object)),
                    "fresh": observed.membership().iter().any(|row| row.members.contains(object)),
                },
            })
        };
        let summarize = |identities: &[&pbps_model::resolver::ObjectIdentity]| {
            serde_json::json!({
                "count": identities.len(),
                "truncated": identities.len() > 8,
                "identities": identities.iter().copied().take(8).map(&describe).collect::<Vec<_>>(),
            })
        };
        let generated_object = generation_objects(&table, "g")[1].clone();
        let generated_bindings: Vec<_> = [
            ("opening", evidence.before()),
            ("desired", evidence.after()),
            ("fresh", &observed),
        ]
        .into_iter()
        .map(|(side, manifest)| {
            let record = manifest
                .prerequisites()
                .iter()
                .find(|row| row.object == generated_object);
            let count = record.map_or(0, |row| row.bindings.len());
            serde_json::json!({
                "side": side,
                "surface": Surface::Default(table.column("g")),
                "object": generated_object,
                "present": record.is_some(),
                "ownership": record.map(|row| &row.ownership),
                "count": count,
                "truncated": count > 8,
                "bindings": record.into_iter().flat_map(|row| &row.bindings).take(8).collect::<Vec<_>>(),
            })
        })
        .collect();
        eprintln!(
            "PBPS1274_GENERATION_INVENTORY {}",
            serde_json::json!({
                "diagnostic": "pbps-generated-inventory",
                "case": case_name,
                "phase": "closing-inventory",
                "expected_records": expected.len(),
                "observed_records": observed.prerequisites().len(),
                "expected_identities": expected_records.len(),
                "observed_identities": observed_records.len(),
                "common_identities": common.len(),
                "expected_minus_observed": summarize(&missing),
                "observed_minus_expected": summarize(&unexpected),
                "properties_mismatches": summarize(&properties_mismatches),
                "bindings_mismatches": summarize(&bindings_mismatches),
                "generated_default_bindings": generated_bindings,
            })
        );
    }
    assert_eq!(
        expected.len(),
        observed.prerequisites().len(),
        "complete fresh closing inventory"
    );
    for record in expected {
        let actual = observed
            .prerequisites()
            .iter()
            .find(|row| row.object == record.object)
            .unwrap();
        assert_eq!(
            record.properties, actual.properties,
            "closing properties: {:?}",
            record.object
        );
        assert_eq!(
            record.bindings, actual.bindings,
            "closing bindings: {:?}",
            record.object
        );
    }
    assert_eq!(before.as_array().unwrap().len(), 3);
    assert_eq!(
        after.as_array().unwrap().len(),
        if set_expression { 3 } else { 4 }
    );
    for old in before.as_array().unwrap() {
        let new = after
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["name"] == old["name"])
            .unwrap();
        assert_eq!(old["table_oid"], new["table_oid"]);
        assert_eq!(old["column_number"], new["column_number"]);
        if old["name"] == "g" && set_expression {
            assert_ne!(
                old["attrdef_oid"], new["attrdef_oid"],
                "actual SET EXPRESSION replaces attrdef"
            );
        }
        if old["name"] == "d" {
            assert_eq!(old["attrdef_oid"], new["attrdef_oid"]);
        }
    }
    for inventory in [&before, &after] {
        for row in inventory
            .as_array()
            .unwrap()
            .iter()
            .filter(|row| row["name"] == "g" || row["name"] == "h")
        {
            assert_eq!(row["generated"], "s");
            assert!(row["attrdef_oid"].as_i64().unwrap() > 0);
            assert_eq!(row["internal_owner"], 1);
            assert_eq!(row["input_reference"], 1);
        }
    }
    assert_eq!(before_values, serde_json::json!([5, 11, 7]));
    assert_eq!(
        after_values,
        if set_expression {
            serde_json::json!([5, 15, 7])
        } else {
            serde_json::json!([5, 11, 7, 9])
        }
    );
    eprintln!(
        "PBPS1274_GENERATION {}",
        serde_json::json!({
            "case": case_name,
            "phase": "passed",
            "properties": [
                "qualified-ordinary-target-and-scratch",
                "exact-generated-default-ownership-and-inventory",
                "retained-recorded-table-and-column-uids",
                "actual-typed-change",
                "adapter-reader",
                "saved-plan-roundtrip",
                "typed-apply",
                "complete-fresh-closing-capture",
                "actual-catalog-and-values",
                "normal-product-cleanup-and-restored-membership",
            ],
        })
    );
}

#[tokio::test]
#[ignore = "requires pinned native PostgreSQL target and owned Docker fixture"]
async fn the_pg18_producer_projects_the_replaced_generated_attrdef_and_preserves_column_identity() {
    generation_producer_case(GenerationCase::SetExpression).await;
}

#[tokio::test]
#[ignore = "requires pinned native PostgreSQL target and owned Docker fixture"]
async fn the_pg16_producer_adds_stored_generation_and_retains_existing_generated_bindings() {
    generation_producer_case(GenerationCase::AddStored).await;
}

mod column_vector_parent {
    use super::*;
    use pbps_model::{Column, Generated, PlannedChange, TableName};
    use pbps_pg::resolver::capture::BindingRecord;

    fn derive(
        changes: &ChangeSet,
        base: &Schema,
        base_ids: &IdsFile,
        desired: &Schema,
        desired_ids: &IdsFile,
        opening: &[BindingRecord],
        compiled: &[BindingRecord],
    ) -> Vec<pbps_model::resolver::ObjectTransition> {
        super::super::transitions::derive(
            changes,
            pbps_diff::Side {
                schema: base,
                ids: base_ids,
            },
            pbps_diff::Side {
                schema: desired,
                ids: desired_ids,
            },
            opening,
            compiled,
        )
        .unwrap()
    }

    fn without_generated(table: &TableName, name: &str) -> Vec<BindingRecord> {
        generation_records(table, "g")
            .into_iter()
            .filter(|record| {
                !matches!(
                    &record.ownership,
                    ObjectOwnership::Surface(Surface::Column(c) | Surface::Default(c))
                        if c.name == name
                )
            })
            .collect()
    }

    #[test]
    fn vector_changes_account_for_the_exact_parent_and_only_the_changed_column() {
        let table = TableName::new("app", "t");
        let base = generation_schema(&table);
        let base_ids = ids(&base, &IdsFile::default());
        let opening = generation_records(&table, "g");
        let relation = generation_objects(&table, "g")[0].signature[0].clone();
        for kind in ["plain", "default", "generated", "drop", "rename"] {
            let mut desired = base.clone();
            let (change, surface, before, after, compiled, desired_ids) = match kind {
                "drop" => {
                    desired
                        .tables
                        .get_mut(&table)
                        .unwrap()
                        .columns
                        .shift_remove("g");
                    let uid = base_ids.column_uid(&table.column("g")).unwrap().clone();
                    let mut desired_ids = base_ids.clone();
                    desired_ids.columns.remove(&uid);
                    let objects = generation_objects(&table, "g");
                    (
                        Change::DropColumn {
                            uid,
                            column: table.column("g"),
                        },
                        Surface::Column(table.column("g")),
                        BTreeSet::from(objects),
                        BTreeSet::new(),
                        without_generated(&table, "g"),
                        desired_ids,
                    )
                }
                "rename" => {
                    let definition = desired.tables.get_mut(&table).unwrap();
                    let column = definition.columns.shift_remove("g").unwrap();
                    definition.columns.insert("h".into(), column);
                    let uid = base_ids.column_uid(&table.column("g")).unwrap().clone();
                    let mut desired_ids = base_ids.clone();
                    desired_ids.columns.get_mut(&uid).unwrap().name = "h".into();
                    (
                        Change::RenameColumn {
                            uid,
                            table: table.clone(),
                            from: "g".into(),
                            to: "h".into(),
                            table_was: None,
                        },
                        Surface::Column(table.column("h")),
                        BTreeSet::from(generation_objects(&table, "g")),
                        BTreeSet::from(generation_objects(&table, "h")),
                        generation_records(&table, "h"),
                        desired_ids,
                    )
                }
                _ => {
                    let mut column = Column::new("integer".parse().unwrap());
                    if kind == "default" {
                        column.default = Some("7".into());
                    } else if kind == "generated" {
                        column.generated = Some(Generated {
                            expression: "a + 4".into(),
                            stored: true,
                        });
                    }
                    desired
                        .tables
                        .get_mut(&table)
                        .unwrap()
                        .columns
                        .insert("h".into(), column.clone());
                    let desired_ids = ids(&desired, &base_ids);
                    let [child, attrdef, mut owner, reference] = generation_objects(&table, "h");
                    let mut additions = vec![BindingRecord {
                        object: child.clone(),
                        ownership: ObjectOwnership::Surface(Surface::Column(table.column("h"))),
                        bindings: vec![],
                    }];
                    if kind != "plain" {
                        if kind == "default" {
                            owner.name = vec!["a".into()];
                        }
                        for object in [attrdef, owner] {
                            additions.push(BindingRecord {
                                object,
                                ownership: ObjectOwnership::Surface(Surface::Default(
                                    table.column("h"),
                                )),
                                bindings: vec![],
                            });
                        }
                        if kind == "generated" {
                            additions.push(BindingRecord {
                                object: reference,
                                ownership: ObjectOwnership::Surface(Surface::Default(
                                    table.column("h"),
                                )),
                                bindings: vec![],
                            });
                        }
                    }
                    let after = additions.iter().map(|r| r.object.clone()).collect();
                    let mut compiled = opening.clone();
                    compiled.extend(additions);
                    (
                        Change::AddColumn {
                            uid: desired_ids.column_uid(&table.column("h")).unwrap().clone(),
                            table: table.clone(),
                            name: "h".into(),
                            column: Box::new(column),
                        },
                        Surface::Column(table.column("h")),
                        BTreeSet::new(),
                        after,
                        compiled,
                        desired_ids,
                    )
                }
            };
            let transitions = derive(
                &ChangeSet {
                    changes: vec![PlannedChange::new(change)],
                },
                &base,
                &base_ids,
                &desired,
                &desired_ids,
                &opening,
                &compiled,
            );
            assert_eq!(transitions.len(), 2, "{kind}");
            let parent = transitions
                .iter()
                .find(|t| t.surface == Surface::Table(table.clone()))
                .unwrap();
            assert_eq!(parent.before, BTreeSet::from([relation.clone()]), "{kind}");
            assert_eq!(parent.after, BTreeSet::from([relation.clone()]), "{kind}");
            let column = transitions.iter().find(|t| t.surface == surface).unwrap();
            assert_eq!(column.before, before, "{kind}");
            assert_eq!(column.after, after, "{kind}");
            for inventory in [
                transitions
                    .iter()
                    .flat_map(|t| &t.before)
                    .collect::<Vec<_>>(),
                transitions
                    .iter()
                    .flat_map(|t| &t.after)
                    .collect::<Vec<_>>(),
            ] {
                assert_eq!(
                    inventory.len(),
                    inventory.iter().collect::<BTreeSet<_>>().len(),
                    "{kind}"
                );
            }
        }
    }

    #[test]
    fn vector_parent_endpoints_follow_table_uids_when_an_opening_name_is_reused() {
        let old = TableName::new("app", "a");
        let renamed = TableName::new("app", "b");
        let other = TableName::new("app", "c");
        let mut base = generation_schema(&old);
        base.tables.extend(generation_schema(&other).tables);
        let base_ids = ids(&base, &IdsFile::default());
        let mut desired = Schema::default();
        let mut first = base.tables[&old].clone();
        first.columns.shift_remove("g");
        desired.tables.insert(renamed.clone(), first);
        desired
            .tables
            .insert(old.clone(), base.tables[&other].clone());
        let mut desired_ids = base_ids.clone();
        desired_ids.rename_table(&old, &renamed);
        desired_ids.rename_table(&other, &old);
        desired_ids
            .columns
            .remove(base_ids.column_uid(&old.column("g")).unwrap());
        let changes = ChangeSet {
            changes: vec![
                PlannedChange::new(Change::DropColumn {
                    uid: base_ids.column_uid(&old.column("g")).unwrap().clone(),
                    column: old.column("g"),
                }),
                PlannedChange::new(Change::RenameTable {
                    uid: base_ids.table_uid(&old).unwrap().clone(),
                    from: old.clone(),
                    to: renamed.clone(),
                    defaults: vec!["d".into()],
                }),
                PlannedChange::new(Change::RenameTable {
                    uid: base_ids.table_uid(&other).unwrap().clone(),
                    from: other.clone(),
                    to: old.clone(),
                    defaults: vec!["d".into(), "g".into()],
                }),
            ],
        };
        let mut opening = generation_records(&old, "g");
        opening.extend(generation_records(&other, "g"));
        let mut compiled = without_generated(&renamed, "g");
        compiled.extend(generation_records(&old, "g"));
        let transitions = derive(
            &changes,
            &base,
            &base_ids,
            &desired,
            &desired_ids,
            &opening,
            &compiled,
        );
        let dropped = transitions
            .iter()
            .find(|t| t.surface == Surface::Column(old.column("g")))
            .unwrap();
        assert_eq!(
            dropped.before,
            BTreeSet::from(generation_objects(&old, "g"))
        );
        assert!(dropped.after.is_empty());
        for (final_table, opening_table) in [(&renamed, &old), (&old, &other)] {
            let parent = transitions
                .iter()
                .find(|t| t.surface == Surface::Table(final_table.clone()))
                .unwrap();
            let relation = generation_objects(opening_table, "g")[0].signature[0].clone();
            assert!(parent.before.contains(&relation));
            let wrong = generation_objects(if opening_table == &old { &other } else { &old }, "g")
                [0]
            .signature[0]
                .clone();
            assert!(!parent.before.contains(&wrong));
        }
        for (actual, expected) in [
            (
                transitions
                    .iter()
                    .flat_map(|t| &t.before)
                    .collect::<Vec<_>>(),
                &opening,
            ),
            (
                transitions
                    .iter()
                    .flat_map(|t| &t.after)
                    .collect::<Vec<_>>(),
                &compiled,
            ),
        ] {
            let unique = actual.iter().copied().collect::<BTreeSet<_>>();
            assert_eq!(actual.len(), unique.len());
            assert_eq!(unique, expected.iter().map(|r| &r.object).collect());
        }
    }

    #[test]
    fn nonvector_column_edits_do_not_acquire_parent_inventory() {
        let table = TableName::new("app", "t");
        let base = generation_schema(&table);
        let ids = ids(&base, &IdsFile::default());
        let uid = ids.column_uid(&table.column("g")).unwrap().clone();
        let records = generation_records(&table, "g");
        for change in [
            Change::AlterColumnType {
                uid: uid.clone(),
                column: table.column("g"),
                from: "integer".parse().unwrap(),
                to: "bigint".parse().unwrap(),
                from_nullable: true,
                to_nullable: true,
                from_collation: None,
                to_collation: None,
            },
            Change::AlterColumnNullability {
                uid: uid.clone(),
                column: table.column("g"),
                ty: "integer".parse().unwrap(),
                to_nullable: false,
                collation: None,
            },
            Change::AlterColumnExpression {
                uid,
                column: table.column("g"),
                from: "a * 2 + 1".into(),
                to: "a * 3".into(),
            },
            Change::AlterColumnDefault {
                uid: ids.column_uid(&table.column("d")).unwrap().clone(),
                column: table.column("d"),
                from: Some("7".into()),
                to: Some("8".into()),
            },
        ] {
            let changes = ChangeSet {
                changes: vec![PlannedChange::new(change)],
            };
            let transitions = derive(&changes, &base, &ids, &base, &ids, &records, &records);
            assert_eq!(transitions.len(), 1);
            assert!(!matches!(transitions[0].surface, Surface::Table(_)));
            let relation = generation_objects(&table, "g")[0].signature[0].clone();
            assert!(!transitions[0].before.contains(&relation));
            assert!(!transitions[0].after.contains(&relation));
        }
    }
}
