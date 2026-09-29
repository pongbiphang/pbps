//! Independent native oracles for the container arm of shared scratch analysis.
//!
//! The #613 overload pair is an engine-observed binding change. The other
//! cases keep exact resource names so refusal and cancelled awaits cannot
//! silently become drop-only cleanup.

use super::*;
use crate::resolver::docker::CandidateSession;
use pbps_config::resolver::{PullPolicy, ResolverProfile};
use pbps_db::resolver::capture::Verdict as BindingVerdict;
use pbps_db::transport::PeerVerifiedConn;
use pbps_model::{IdsFile, Module, ModuleKind, Schema};
use serde_json::Value;
use std::collections::BTreeMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use tokio::sync::Notify;
use tokio::time::{Duration, Instant};

const SCHEMA: &str = "pbps_bind1273";

fn fixture() -> (PathBuf, &'static str, u32) {
    assert_eq!(
        std::env::var("PBPS_NATIVE_FACTORY_FIXTURE").as_deref(),
        Ok("1")
    );
    assert_eq!(std::env::var("PBPS_NATIVE_DRIVER").as_deref(), Ok("pg"));
    let (image, version) = match std::env::var("PBPS_NATIVE_PG_MAJOR").unwrap().as_str() {
        "16" => (
            "postgres@sha256:485935f94cc7165afa896978809c37b592dc07f0a37d2c8f645f12412d0212c8",
            160015,
        ),
        "18" => (
            "postgres@sha256:4ef4dbc939d61acea57712655ddb4b4ab27419c913f94cca0cd57cb3ea3c2280",
            180006,
        ),
        other => panic!("unmeasured PostgreSQL fixture {other}"),
    };
    assert_eq!(
        std::env::var("PBPS_RESOLVER_TEST_IMAGE").as_deref(),
        Ok(image)
    );
    (
        PathBuf::from(std::env::var("PBPS_RESOLVER_TEST_SOCKET").unwrap()),
        image,
        version,
    )
}

fn docker(args: &[&str]) -> String {
    let binary = std::env::var("PBPS_NATIVE_DOCKER").unwrap();
    assert!(Path::new(&binary).is_absolute());
    let socket = std::env::var("PBPS_RESOLVER_TEST_SOCKET").unwrap();
    let reply = Command::new(binary)
        .arg("--host")
        .arg(format!("unix://{socket}"))
        .args(args)
        .output()
        .unwrap();
    assert!(
        reply.status.success(),
        "owned fixture Docker observation {:?}: {}",
        args,
        String::from_utf8_lossy(&reply.stderr)
    );
    String::from_utf8(reply.stdout).unwrap()
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

fn created_since(before: &BTreeMap<String, String>) -> BTreeMap<String, String> {
    containers()
        .into_iter()
        .filter(|(id, _)| !before.contains_key(id))
        .collect()
}

fn inspect(id: &str) -> Value {
    serde_json::from_str::<Value>(&docker(&["inspect", "--type", "container", id])).unwrap()[0]
        .clone()
}

fn workload(candidates: &BTreeMap<String, String>) -> (String, u32) {
    let matches: Vec<_> = candidates
        .keys()
        .filter_map(|id| {
            let view = inspect(id);
            (view["HostConfig"]["NetworkMode"] == "none").then(|| {
                (
                    id.clone(),
                    view["State"]["Pid"].as_u64().unwrap().try_into().unwrap(),
                )
            })
        })
        .collect();
    assert_eq!(matches.len(), 1, "one owned isolated workload: {matches:?}");
    matches.into_iter().next().unwrap()
}

fn cleanup_observation(before: &BTreeMap<String, String>, recovery_names: &[String]) {
    let remaining = created_since(before);
    let unnamed: Vec<_> = remaining
        .values()
        .filter(|name| !recovery_names.contains(name))
        .cloned()
        .collect();
    // Fixture cleanup is confined to immutable IDs observed as new in this
    // case. The assertion still fails if the product left one unnamed.
    for id in remaining.keys() {
        let _ = docker(&["rm", "--force", "--volumes", id]);
    }
    assert!(
        unnamed.is_empty(),
        "owned containers neither removed nor named: {unnamed:?}"
    );
    assert!(created_since(before).is_empty());
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

async fn candidate(target: &mut NativeTarget, socket: &Path, image: &str) -> CandidateSession {
    let mut api = LocalApi::connect_native(socket).await.unwrap();
    let acquired = api
        .acquire(&ResolverProfile::Docker {
            image: image.into(),
            pull: PullPolicy::Never,
        })
        .await
        .unwrap();
    CandidateSession::start(api, acquired, target)
        .await
        .expect("the pinned native container qualifies before analysis")
}

async fn candidate_with_start_window(
    target: &mut NativeTarget,
    socket: &Path,
    image: &str,
) -> (CandidateSession, Instant, Instant) {
    let mut api = LocalApi::connect_native(socket).await.unwrap();
    let acquired = api
        .acquire(&ResolverProfile::Docker {
            image: image.into(),
            pull: PullPolicy::Never,
        })
        .await
        .unwrap();
    // Bracket the public startup call, not an implementation deadline getter.
    let before_start = Instant::now();
    let candidate = CandidateSession::start(api, acquired, target)
        .await
        .expect("the pinned native container qualifies before analysis");
    let after_start = Instant::now();
    (candidate, before_start, after_start)
}

tokio::task_local! {
    static DEADLINE_NOW: Instant;
}

/// Only the analysis deadline comparison reads this injected instant. Docker
/// supervision, root guards, timeouts and both engines keep their real clocks.
pub(crate) fn deadline_now() -> Instant {
    DEADLINE_NOW
        .try_with(|now| *now)
        .unwrap_or_else(|_| Instant::now())
}

fn declared(modules: &[(&str, ModuleKind, &str)]) -> Schema {
    let mut schema = Schema::default();
    for &(id, kind, definition) in modules {
        schema.modules.insert(
            id.parse().unwrap(),
            Module {
                kind,
                description: None,
                definition: definition.into(),
            },
        );
    }
    schema
}

fn bootstrap(desired: &Schema) -> Vec<pbps_model::Change> {
    let context = pbps_diff::Context {
        operator: "fixture".into(),
        today: "2026-09-25".into(),
    };
    let ids = pbps_diff::resolve(desired, &IdsFile::default(), &[], &context)
        .unwrap()
        .ids;
    let empty = Schema::default();
    pbps_diff::diff(
        pbps_diff::Side {
            schema: &empty,
            ids: &IdsFile::default(),
        },
        pbps_diff::Side {
            schema: desired,
            ids: &ids,
        },
        &pbps_pg::Postgres::new(),
        &pbps_model::Hints::default(),
    )
    .unwrap()
    .changes
    .into_iter()
    .map(|planned| planned.change)
    .collect()
}

fn surface(
    assessment: &pbps_db::resolver::capture::Assessment,
    relation: &str,
) -> Option<BindingVerdict> {
    assessment
        .surfaces
        .iter()
        .find(|(object, _)| {
            object.class == "pg_rewrite"
                && object
                    .signature
                    .first()
                    .is_some_and(|name| name.name == [SCHEMA, relation])
        })
        .map(|(_, verdict)| verdict.clone())
}

async fn actual_target_version(expected: u32) {
    let mut peer = PeerVerifiedConn::connect(
        Driver::Postgres,
        &std::env::var("PBPS_NATIVE_CONNECTION").unwrap(),
    )
    .await
    .unwrap();
    let rows = peer
        .query("SELECT current_setting('server_version_num') AS version_num")
        .await
        .unwrap();
    let reported: u32 = rows[0]
        .try_get::<&str>("version_num")
        .unwrap()
        .unwrap()
        .parse()
        .unwrap();
    assert_eq!(
        reported, expected,
        "the connected target is the pinned engine"
    );
}

#[tokio::test]
#[ignore = "requires the owned native Docker daemon and a pinned PostgreSQL TLS target"]
async fn the_owned_container_resolves_the_overload_pair_on_its_qualified_connections() {
    let (socket, image, version) = fixture();
    actual_target_version(version).await;
    let numeric = (
        "pbps_bind1273.f(numeric)",
        ModuleKind::Function,
        "(numeric) RETURNS numeric LANGUAGE sql IMMUTABLE RETURN $1",
    );
    let integer = (
        "pbps_bind1273.f(integer)",
        ModuleKind::Function,
        "(integer) RETURNS numeric LANGUAGE sql IMMUTABLE RETURN $1",
    );
    let affected = ("pbps_bind1273.v", ModuleKind::View, "SELECT f(1) AS x");
    let control = (
        "pbps_bind1273.control",
        ModuleKind::View,
        "SELECT f(1::numeric) AS x",
    );
    let base = declared(&[numeric, affected, control]);
    let desired = declared(&[numeric, integer, affected, control]);
    let bootstrap = bootstrap(&desired);
    let request = BindingRequest {
        bootstrap: &bootstrap,
        desired: &desired,
        base: &base,
    };
    // Only the fixture seeds the target; the product's qualification and
    // resolution use read-only target connections.
    let mut setup = PeerVerifiedConn::connect(
        Driver::Postgres,
        &std::env::var("PBPS_NATIVE_CONNECTION").unwrap(),
    )
    .await
    .unwrap();
    for statement in [
        "DROP SCHEMA IF EXISTS pbps_bind1273 CASCADE",
        "CREATE SCHEMA pbps_bind1273",
        "CREATE FUNCTION pbps_bind1273.f(numeric) RETURNS numeric LANGUAGE sql IMMUTABLE RETURN $1",
        "CREATE VIEW pbps_bind1273.v AS SELECT pbps_bind1273.f(1) AS x",
        "CREATE VIEW pbps_bind1273.control AS SELECT pbps_bind1273.f(1::numeric) AS x",
    ] {
        setup.query(statement).await.unwrap();
    }
    let before = containers();
    let mut target = target().await;
    let mut candidate = candidate(&mut target, &socket, image).await;
    let recipe = target.database_recipe().await.unwrap();
    let mut unqualified_run = candidate
        .open_scratch(&recipe)
        .await
        .expect("prequalification refusal has its own owned run");
    let unqualified = unqualified_run
        .resolve(&mut target, &request)
        .await
        .expect_err("declarations cannot cross before qualification");
    assert!(
        unqualified.to_string().contains("not been qualified"),
        "{unqualified}"
    );
    let refused_cleanup = unqualified_run.close().await;
    let refused_recovery = refused_cleanup
        .as_ref()
        .err()
        .map(|failure| failure.recovery_names.clone())
        .unwrap_or_default();
    cleanup_observation(&before, &refused_recovery);
    target.check().await.unwrap();
    let before = containers();
    let mut candidate = self::candidate(&mut target, &socket, image).await;
    let mut run = candidate
        .open_scratch(&recipe)
        .await
        .expect("the positive binding oracle uses a fresh candidate and run");
    assert_eq!(
        run.qualify(
            &mut target,
            &ScopeRequest {
                schemas: vec![SCHEMA.into()],
                ..ScopeRequest::default()
            },
        )
        .await
        .unwrap(),
        Verdict::Verified
    );
    let (target_connection, scratch_connection) = run.scope_connections().unwrap();
    assert_eq!(target_connection, target.connection_id().unwrap());
    assert_ne!(target_connection, scratch_connection);
    let assessment = run.resolve(&mut target, &request).await.unwrap();
    assert_eq!(surface(&assessment, "v"), Some(BindingVerdict::Rebuild));
    assert_eq!(
        surface(&assessment, "control"),
        Some(BindingVerdict::Unaffected),
        "the qualified numeric call remains bound to the original overload: {assessment:?}"
    );
    let again = run.resolve(&mut target, &request).await.unwrap_err();
    assert!(again.to_string().contains("fresh run"), "{again}");
    let closed = run.close().await;
    run.close().await.expect("completed cleanup is idempotent");
    let recovery = closed
        .as_ref()
        .err()
        .map(|failure| failure.recovery_names.clone())
        .unwrap_or_default();
    cleanup_observation(&before, &recovery);
    closed.expect("successful analysis removes every transferred resource");
    target.check().await.unwrap();
    setup
        .query("DROP SCHEMA pbps_bind1273 CASCADE")
        .await
        .unwrap();
}

#[tokio::test]
#[ignore = "requires the owned native Docker daemon and a pinned PostgreSQL TLS target"]
async fn delayed_container_analysis_expires_at_its_first_owner_bound() {
    let (socket, image, version) = fixture();
    actual_target_version(version).await;
    let before = containers();
    let mut target = target().await;
    let (mut candidate, before_start, after_start) =
        candidate_with_start_window(&mut target, &socket, image).await;
    let (workload_id, _) = workload(&created_since(&before));
    let recipe = target.database_recipe().await.unwrap();

    // Hold the already-started candidate briefly on the real scheduler.
    // Scratch opening therefore occurs strictly after the first owner's
    // startup window, without waiting for the 600-second production bound.
    tokio::time::sleep(Duration::from_millis(25)).await;
    let mut run = candidate
        .open_scratch(&recipe)
        .await
        .expect("a held but live candidate can still open scratch");

    // This instant precedes any possible first-owner deadline: startup began
    // only after before_start. Real engine and channel checks still execute.
    let before_bound = before_start + Duration::from_secs(599);
    let positive = DEADLINE_NOW
        .scope(before_bound, async {
            run.check(&mut target).await?;
            run.qualify(&mut target, &ScopeRequest::default()).await
        })
        .await;

    // Startup completed by after_start, so every honest first-owner bound
    // has expired here. A new 600-second budget minted after the real hold
    // and scratch opening remains live, making this a behavioral distinction
    // rather than a field comparison. Only analysis comparisons see the
    // injected instant; the native runtime and scheduler keep real time.
    let (at_bound, still_running, terminal) = if positive.is_ok() {
        let result = DEADLINE_NOW
            .scope(
                after_start + Duration::from_secs(600),
                run.check(&mut target),
            )
            .await;
        let still_running = inspect(&workload_id)["State"]["Running"].as_bool() == Some(true);
        let terminal = if matches!(&result, Err(Error::Deadline)) {
            let checked = DEADLINE_NOW.scope(before_bound, run.check(&mut target)).await;
            let qualified = DEADLINE_NOW
                .scope(
                    before_bound,
                    run.qualify(&mut target, &ScopeRequest::default()),
                )
                .await;
            matches!(checked, Err(Error::Deadline))
                && matches!(qualified, Err(Error::Deadline))
        } else {
            false
        };
        (Some(result), still_running, terminal)
    } else {
        (None, false, false)
    };

    let closed = run.close().await;
    if closed.is_ok() {
        run.close().await.expect("completed cleanup is idempotent");
    }
    let recovery = closed
        .as_ref()
        .err()
        .map(|failure| failure.recovery_names.clone())
        .unwrap_or_default();
    cleanup_observation(&before, &recovery);
    closed.expect("deadline refusal removes every transferred resource");
    target.check().await.expect("the target is still usable");

    assert_eq!(positive.unwrap(), Verdict::Verified);
    assert!(
        still_running,
        "the real owned workload must still be live at the injected analysis bound"
    );
    assert!(
        matches!(&at_bound, Some(Err(Error::Deadline))),
        "the first-owner deadline must refuse before a fresh scratch budget: {at_bound:?}"
    );
    assert!(terminal, "deadline refusal cannot regain a qualified scope");
}

fn established_pg_channels(root: u32) -> usize {
    std::fs::read_to_string(format!("/proc/{root}/net/tcp"))
        .unwrap()
        .lines()
        .skip(1)
        .filter(|line| {
            let fields: Vec<_> = line.split_whitespace().collect();
            fields.len() > 3
                && fields[3] == "01"
                && (fields[1].ends_with(":1538") || fields[2].ends_with(":1538"))
        })
        .count()
}

struct Foreign(Option<String>);

impl Foreign {
    async fn open(workload: &str, root: u32, baseline: usize) -> Self {
        let name = format!("pbps-foreign-{:032x}", rand::random::<u128>());
        let network = format!("container:{workload}");
        let image = std::env::var("PBPS_RESOLVER_TEST_IMAGE").unwrap();
        let id = docker(&[
            "create",
            "--name",
            &name,
            "--pull",
            "never",
            "--network",
            &network,
            "--entrypoint",
            "/bin/bash",
            &image,
            "-ec",
            "exec 3<>/dev/tcp/127.0.0.1/5432; sleep 120",
        ])
        .trim()
        .to_owned();
        let foreign = Self(Some(id));
        let _ = docker(&["start", foreign.0.as_deref().unwrap()]);
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                if established_pg_channels(root) >= baseline + 2 {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
        })
        .await
        .expect("the foreign fixture must establish an actual extra socket pair");
        foreign
    }

    fn close(mut self) {
        if let Some(id) = self.0.take() {
            let _ = docker(&["rm", "--force", "--volumes", &id]);
        }
    }
}

impl Drop for Foreign {
    fn drop(&mut self) {
        let Some(id) = self.0.as_deref() else {
            return;
        };
        let binary = std::env::var("PBPS_NATIVE_DOCKER").unwrap();
        let socket = std::env::var("PBPS_RESOLVER_TEST_SOCKET").unwrap();
        let _ = Command::new(binary)
            .args([
                "--host",
                &format!("unix://{socket}"),
                "rm",
                "--force",
                "--volumes",
                id,
            ])
            .output();
    }
}

#[tokio::test]
#[ignore = "requires the owned native Docker daemon and a pinned PostgreSQL TLS target"]
async fn a_changed_owned_runtime_or_target_ends_container_analysis_permanently() {
    let (socket, image, version) = fixture();
    actual_target_version(version).await;
    for cause in ["foreign-channel", "target-rebound"] {
        let before = containers();
        let mut target = target().await;
        let mut candidate = candidate(&mut target, &socket, image).await;
        let candidates = created_since(&before);
        let (workload, root) = workload(&candidates);
        let recipe = target.database_recipe().await.unwrap();
        let mut run = candidate.open_scratch(&recipe).await.unwrap();
        assert_eq!(
            run.qualify(&mut target, &ScopeRequest::default())
                .await
                .unwrap(),
            Verdict::Verified
        );
        let first = if cause == "foreign-channel" {
            let baseline = established_pg_channels(root);
            let foreign = Foreign::open(&workload, root, baseline).await;
            let result = run.check(&mut target).await;
            foreign.close();
            result
        } else {
            let peer = PeerVerifiedConn::connect(
                Driver::Postgres,
                &std::env::var("PBPS_NATIVE_CONNECTION").unwrap(),
            )
            .await
            .unwrap();
            let mut replacement = NativeTarget::establish(
                peer,
                std::env::var("PBPS_NATIVE_SERVICE_PID")
                    .unwrap()
                    .parse()
                    .unwrap(),
            )
            .await
            .unwrap();
            assert!(target.same_instance(&mut replacement).await.unwrap());
            run.check(&mut replacement).await
        };
        let remains_terminal = run.check(&mut target).await.is_err()
            && run
                .qualify(&mut target, &ScopeRequest::default())
                .await
                .is_err();
        let closed = run.close().await;
        run.close().await.expect("refused cleanup is idempotent");
        let recovery = closed
            .as_ref()
            .err()
            .map(|failure| failure.recovery_names.clone())
            .unwrap_or_default();
        cleanup_observation(&before, &recovery);
        assert!(first.is_err(), "{cause} must invalidate the shared run");
        assert!(remains_terminal, "{cause} cannot regain a sealed scope");
        closed.expect("a refused run cleans every transferred resource");
        target.check().await.unwrap();
    }
}

#[derive(Clone)]
struct Pause {
    stage: &'static str,
    reached: Arc<Notify>,
}

tokio::task_local! {
    static PAUSE: Pause;
}

/// Called only from test-gated ownership milestones. A hook signals before
/// waiting forever, so the test cancels the exact future without sleeping or
/// depending on scheduler timing. No task-local value means no pause.
pub(crate) async fn pause(stage: &'static str) {
    let active = PAUSE
        .try_with(|hook| {
            if hook.stage == stage {
                hook.reached.notify_one();
                true
            } else {
                false
            }
        })
        .unwrap_or(false);
    if active {
        std::future::pending::<()>().await;
    }
}

async fn cancel_after<F: Future>(stage: &'static str, future: F) {
    let hook = Pause {
        stage,
        reached: Arc::new(Notify::new()),
    };
    let reached = hook.reached.clone();
    let signal = reached.notified();
    let mut step = Box::pin(PAUSE.scope(hook, future));
    tokio::time::timeout(std::time::Duration::from_secs(60), async {
        tokio::select! {
            _ = signal => {},
            _ = &mut step => panic!("{stage} completed before its owned-resource milestone"),
        }
    })
    .await
    .expect("the task-owned milestone must be reached");
    drop(step);
}

#[tokio::test]
#[ignore = "requires the owned native Docker daemon and a pinned PostgreSQL TLS target"]
async fn cancelled_container_analysis_removes_or_names_every_owned_resource() {
    let (socket, image, version) = fixture();
    actual_target_version(version).await;
    let empty = Schema::default();
    let binding = BindingRequest {
        bootstrap: &[],
        desired: &empty,
        base: &empty,
    };
    for stage in [
        "container-create-owned",
        "container-discard-owned",
        "container-qualify-admin-owned",
        "container-capture-admin-owned",
        "container-close-owned",
    ] {
        let before = containers();
        let mut target = target().await;
        let mut candidate = candidate(&mut target, &socket, image).await;
        let recipe = target.database_recipe().await.unwrap();
        if matches!(stage, "container-create-owned" | "container-discard-owned") {
            cancel_after("container-create-owned", candidate.open_scratch(&recipe)).await;
            let identity_refused = candidate.identity().is_err();
            let check_refused = candidate.check().await.is_err();
            let retried = candidate.open_scratch(&recipe).await;
            let unexpectedly_open = if let Ok(mut unexpected) = retried {
                let _ = unexpected.close().await;
                true
            } else {
                false
            };
            if stage == "container-discard-owned" {
                cancel_after(stage, candidate.discard()).await;
                assert!(candidate.identity().is_err());
                assert!(candidate.check().await.is_err());
                assert!(candidate.open_scratch(&recipe).await.is_err());
            }
            let discarded = candidate.discard().await;
            if discarded.is_ok() {
                candidate
                    .discard()
                    .await
                    .expect("completed discard is idempotent");
            }
            let recovery = discarded
                .as_ref()
                .err()
                .map(|failure| failure.recovery_names.clone())
                .unwrap_or_default();
            cleanup_observation(&before, &recovery);
            assert!(
                !unexpectedly_open,
                "cancelled creation cannot open another run"
            );
            assert!(
                identity_refused && check_refused,
                "cancelled creation is terminal"
            );
            target.check().await.unwrap();
            continue;
        }
        let mut run = candidate.open_scratch(&recipe).await.unwrap();
        if stage == "container-capture-admin-owned" {
            run.qualify(&mut target, &ScopeRequest::default())
                .await
                .unwrap();
        }
        match stage {
            "container-qualify-admin-owned" => {
                cancel_after(stage, run.qualify(&mut target, &ScopeRequest::default())).await;
            }
            "container-capture-admin-owned" => {
                cancel_after(stage, run.resolve(&mut target, &binding)).await;
            }
            "container-close-owned" => {
                cancel_after(stage, run.close()).await;
            }
            _ => unreachable!(),
        }
        let terminal = run.check(&mut target).await.is_err()
            && run
                .qualify(&mut target, &ScopeRequest::default())
                .await
                .is_err()
            && run.resolve(&mut target, &binding).await.is_err();
        let closed = run.close().await;
        if closed.is_ok() {
            run.close().await.expect("completed cleanup is idempotent");
        }
        let recovery = closed
            .as_ref()
            .err()
            .map(|failure| failure.recovery_names.clone())
            .unwrap_or_default();
        cleanup_observation(&before, &recovery);
        assert!(
            terminal,
            "{stage}: cancellation must permanently end analysis"
        );
        target.check().await.unwrap();
    }
}
