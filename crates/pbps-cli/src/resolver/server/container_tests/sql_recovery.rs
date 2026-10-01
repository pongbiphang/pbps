//! SQL recovery follows confirmed owned storage destruction, never lost SQL.

use super::*;
use std::process::Output;
use std::sync::Mutex;

const OWNER: &str = "io.pbps.resolver.owner";

async fn bounded<T>(stage: &str, future: impl Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(60), future)
        .await
        .unwrap_or_else(|_| panic!("1288 native future exceeded its bound: {stage}"))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Scenario {
    Removed,
    WorkloadUnconfirmed,
    RelayUnconfirmed,
}

#[derive(Clone)]
struct Owned {
    id: String,
    name: String,
    owner: String,
    image: String,
}

fn invoke(args: &[&str]) -> Output {
    Command::new(std::env::var("PBPS_NATIVE_DOCKER").unwrap())
        .arg("--host")
        .arg(format!(
            "unix://{}",
            std::env::var("PBPS_RESOLVER_TEST_SOCKET").unwrap()
        ))
        .args(args)
        .output()
        .unwrap()
}

fn view(id: &str) -> Option<Value> {
    let reply = invoke(&["inspect", "--type", "container", id]);
    if !reply.status.success() {
        let error = String::from_utf8_lossy(&reply.stderr).to_ascii_lowercase();
        assert!(
            error.contains("no such object") || error.contains("no such container"),
            "unreadable runtime is not absence: {error}"
        );
        assert_eq!(
            serde_json::from_slice::<Value>(&reply.stdout).unwrap(),
            serde_json::json!([])
        );
        return None;
    }
    let rows: Vec<Value> = serde_json::from_slice(&reply.stdout).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["Id"], id);
    Some(rows.into_iter().next().unwrap())
}

impl Owned {
    fn capture(id: &str, name: &str) -> Self {
        let row = view(id).expect("the new owned resource exists");
        let owner = row["Config"]["Labels"][OWNER].as_str().unwrap().to_owned();
        assert_eq!(name, format!("pbps-resolver-{owner}"));
        assert_eq!(row["Name"], format!("/{name}"));
        let image = row["Image"].as_str().unwrap().to_owned();
        assert!(image.starts_with("sha256:"));
        Self {
            id: id.into(),
            name: name.into(),
            owner,
            image,
        }
    }

    fn held_name(&self) -> String {
        format!("{}-1288-held", self.name)
    }

    fn checked(&self) -> Option<Value> {
        let row = view(&self.id)?;
        assert_eq!(row["Config"]["Labels"][OWNER], self.owner);
        assert_eq!(row["Image"], self.image);
        assert!(
            row["Name"] == format!("/{}", self.name)
                || row["Name"] == format!("/{}", self.held_name()),
            "only the exact declared name-continuity fault is owned"
        );
        Some(row)
    }

    fn rename(&self) {
        let row = self
            .checked()
            .expect("rename must reverify the exact new ID");
        assert_eq!(row["Name"], format!("/{}", self.name));
        docker(&["rename", &self.id, &self.held_name()]);
        assert_eq!(
            self.checked().unwrap()["Name"],
            format!("/{}", self.held_name())
        );
        eprintln!(
            "1288 native name-continuity fault id={} original={} held={} owner={} image={}",
            self.id,
            self.name,
            self.held_name(),
            self.owner,
            self.image
        );
    }

    fn remove_relay(&self) {
        if self.checked().is_some() {
            let reply = invoke(&["rm", "--force", "--volumes", &self.id]);
            eprintln!(
                "1288 exact relay removal id={} exit={:?}",
                self.id,
                reply.status.code()
            );
        }
        // AutoRemove or supervision can win the DELETE race. A fresh exact
        // absence is required even when the fixture command reports failure.
        assert!(
            self.checked().is_none(),
            "the exact control transport was removed"
        );
    }
}

struct Case {
    before: BTreeMap<String, String>,
    owned: BTreeMap<String, Owned>,
}

impl Case {
    fn new() -> Self {
        Self {
            before: containers(),
            owned: BTreeMap::new(),
        }
    }

    fn record(&mut self) {
        for (id, name) in created_since(&self.before) {
            self.owned
                .entry(id.clone())
                .or_insert_with(|| Owned::capture(&id, &name));
        }
    }

    fn startup(&self) -> (Owned, Owned) {
        let candidates: BTreeMap<_, _> = self
            .owned
            .iter()
            .map(|(id, owned)| (id.clone(), owned.name.clone()))
            .collect();
        let (id, pid) = workload(&candidates);
        let owned = self.owned[&id].clone();
        let control: Vec<_> = self
            .owned
            .values()
            .filter(|entry| entry.id != id)
            .cloned()
            .collect();
        assert_eq!(
            control.len(),
            1,
            "candidate startup owns exactly one control relay"
        );
        let row = owned.checked().unwrap();
        assert_eq!(row["HostConfig"]["AutoRemove"], true);
        assert_eq!(row["HostConfig"]["ReadonlyRootfs"], true);
        assert_eq!(row["Mounts"], serde_json::json!([]));
        let storage = if std::env::var("PBPS_NATIVE_PG_MAJOR").unwrap() == "16" {
            "/var/lib/postgresql/data"
        } else {
            "/var/lib/postgresql"
        };
        assert!(
            row["HostConfig"]["Tmpfs"][storage]
                .as_str()
                .unwrap()
                .contains("noexec")
        );
        let mounts = std::fs::read_to_string(format!("/proc/{pid}/mountinfo")).unwrap();
        let private: Vec<_> = mounts
            .lines()
            .filter(|line| line.split_whitespace().nth(4) == Some(storage))
            .collect();
        assert_eq!(private.len(), 1);
        assert!(private[0].contains(" - tmpfs "));
        eprintln!(
            "1288 admitted workload id={} name={} owner={} image={} private_storage={}",
            owned.id, owned.name, owned.owner, owned.image, private[0]
        );
        (owned, control.into_iter().next().unwrap())
    }

    fn independent_relay(&self, workload: &Owned, control: &Owned) -> Owned {
        let mut live: Vec<_> = self
            .owned
            .values()
            .filter_map(|entry| {
                if entry.id == workload.id || entry.id == control.id {
                    return None;
                }
                let row = entry.checked()?;
                (row["State"]["Running"] == true && row["State"]["Paused"] == false)
                    .then(|| (row["Created"].as_str().unwrap().to_owned(), entry.clone()))
            })
            .collect();
        live.sort_by(|a, b| a.0.cmp(&b.0));
        let (created, relay) = live
            .pop()
            .expect("qualification retained its current scratch relay");
        assert!(
            live.last().is_none_or(|previous| previous.0 != created),
            "relay identity is not ambiguous"
        );
        relay
    }

    fn cleanup(&mut self) {
        self.record();
        for owned in self.owned.values() {
            if owned.checked().is_some() {
                let reply = invoke(&["rm", "--force", "--volumes", &owned.id]);
                eprintln!(
                    "1288 exact fixture cleanup id={} exit={:?}",
                    owned.id,
                    reply.status.code()
                );
            }
            assert!(owned.checked().is_none());
        }
        assert!(created_since(&self.before).is_empty());
    }
}

impl Drop for Case {
    fn drop(&mut self) {
        // Property assertions run after explicit cleanup. This fallback also
        // protects the exact renamed/paused IDs on a fixture-stage panic.
        if !std::thread::panicking() {
            return;
        }
        for owned in self.owned.values() {
            let reply = invoke(&["inspect", "--type", "container", &owned.id]);
            if !reply.status.success() {
                continue;
            }
            let Ok(rows) = serde_json::from_slice::<Vec<Value>>(&reply.stdout) else {
                continue;
            };
            if let Some(row) = rows.first()
                && row["Id"] == owned.id
                && row["Image"] == owned.image
                && row["Config"]["Labels"][OWNER] == owned.owner
                && (row["Name"] == format!("/{}", owned.name)
                    || row["Name"] == format!("/{}", owned.held_name()))
            {
                let removed = invoke(&["rm", "--force", "--volumes", &owned.id]);
                eprintln!(
                    "1288 exact panic cleanup id={} exit={:?}",
                    owned.id,
                    removed.status.code()
                );
            }
        }
    }
}

fn sql_names(names: &ScratchNames, roles: &[String]) -> Vec<String> {
    let mut owned = vec![names.database().to_owned(), names.login().to_owned()];
    owned.extend_from_slice(roles);
    owned.sort_unstable();
    owned.dedup();
    owned
}

async fn catalog(
    connection: &mut StreamConn,
    names: &ScratchNames,
    roles: &[String],
) -> Vec<String> {
    let literal = |name: &str| format!("'{}'", name.replace('\'', "''"));
    let mut principals = vec![names.login().to_owned()];
    principals.extend_from_slice(roles);
    principals.sort_unstable();
    principals.dedup();
    let sql = format!(
        "SELECT datname AS owned_name FROM pg_database WHERE datname={} UNION ALL \
         SELECT rolname AS owned_name FROM pg_roles WHERE rolname IN ({}) ORDER BY owned_name",
        literal(names.database()),
        principals
            .iter()
            .map(|name| literal(name))
            .collect::<Vec<_>>()
            .join(",")
    );
    let rows = bounded("existing owned SQL catalog", connection.query(&sql))
        .await
        .expect("read actual owned SQL identities on the existing connection");
    let found: Vec<String> = rows
        .iter()
        .map(|row| {
            row.try_get::<&str>("owned_name")
                .unwrap()
                .unwrap()
                .to_owned()
        })
        .collect();
    eprintln!("1288 existing SQL identities={found:?}");
    found
}

fn paused_sql(workload: &Owned, names: &ScratchNames) {
    let row = workload
        .checked()
        .expect("the renamed workload remains readable");
    assert_eq!(row["State"]["Paused"], true);
    let major = std::env::var("PBPS_NATIVE_PG_MAJOR").unwrap();
    let psql = format!("/usr/lib/postgresql/{major}/bin/psql");
    let reply = invoke(&[
        "exec",
        &workload.id,
        &psql,
        "-h",
        "127.0.0.1",
        "-U",
        names.login(),
        "-d",
        names.database(),
        "-w",
        "-At",
        "-c",
        "SELECT 1",
    ]);
    assert!(!reply.status.success());
    assert!(
        String::from_utf8_lossy(&reply.stderr)
            .to_ascii_lowercase()
            .contains("paused"),
        "an authentication or provisioning failure is not paused SQL"
    );
    eprintln!("1288 unavailable SQL on retained paused id={}", workload.id);
}

async fn observe_unavailable(connection: &mut StreamConn) {
    match tokio::time::timeout(Duration::from_secs(2), connection.query("SELECT 1")).await {
        Ok(Ok(_)) => panic!("the deliberately interrupted SQL transport still answered"),
        Ok(Err(_)) => eprintln!("1288 actual interrupted SQL observation=connection-error"),
        Err(_) => eprintln!("1288 actual interrupted SQL observation=bounded-timeout"),
    }
}

async fn fault(
    scenario: Scenario,
    workload: &Owned,
    control: &Owned,
    relay: Option<&Owned>,
    names: &ScratchNames,
    roles: &[String],
    connection: &mut StreamConn,
) -> Vec<String> {
    let mut survived = Vec::new();
    if scenario == Scenario::WorkloadUnconfirmed {
        workload.rename();
        workload.checked().unwrap();
        docker(&["pause", &workload.id]);
        paused_sql(workload, names);
        docker(&["unpause", &workload.id]);
        survived = catalog(connection, names, roles).await;
        assert_eq!(workload.checked().unwrap()["State"]["Running"], true);
    }
    if scenario == Scenario::RelayUnconfirmed {
        let relay = relay.expect("the exact independent relay is observed");
        relay.rename();
        let row = relay.checked().unwrap();
        assert_eq!(row["HostConfig"]["AutoRemove"], true);
        assert_eq!(row["State"]["Running"], true);
        // Exit would trigger AutoRemove. Freeze this exact renamed owner so
        // its native name-continuity refusal remains observable after cleanup.
        docker(&["pause", &relay.id]);
        let held = relay.checked().unwrap();
        assert_eq!(held["State"]["Running"], true);
        assert_eq!(held["State"]["Paused"], true);
    }
    if scenario != Scenario::WorkloadUnconfirmed {
        // Pause before interrupting the control transport. A supervisor may
        // already have removed the workload after observing the paused relay.
        // Preserve that exact observation; only CandidateRun::close's retained
        // cleanup result can authorize SQL completion.
        if workload.checked().is_some() {
            let reply = invoke(&["pause", &workload.id]);
            match workload.checked() {
                Some(row) => {
                    assert!(reply.status.success(), "the live exact workload must pause");
                    assert_eq!(row["State"]["Paused"], true);
                    eprintln!("1288 workload pause observation=paused id={}", workload.id);
                }
                None => eprintln!(
                    "1288 workload pause observation=confirmed-exact-id-absence id={}",
                    workload.id
                ),
            }
        } else {
            eprintln!(
                "1288 workload before pause observation=confirmed-exact-id-absence id={}",
                workload.id
            );
        }
    }
    let remove_control =
        scenario != Scenario::RelayUnconfirmed || relay.is_none_or(|relay| relay.id != control.id);
    if remove_control {
        control.remove_relay();
    }
    eprintln!(
        "1288 control transport removed={remove_control} id={} paused_control_is_not_eof={}",
        control.id, !remove_control
    );
    if scenario != Scenario::WorkloadUnconfirmed {
        observe_unavailable(connection).await;
    }
    survived
}

#[derive(Clone)]
struct Observation {
    scenario: Scenario,
    workload: Owned,
    control: Owned,
    created: Arc<Mutex<Option<Created>>>,
}

struct Created {
    names: ScratchNames,
    before: Vec<String>,
    survived: Vec<String>,
}

tokio::task_local! {
    static OBSERVATION: Observation;
}

/// Inactive in every ordinary test. Observe only the already admitted control
/// stream: an additional backend would change the admission census.
pub(crate) async fn scratch_created(names: &ScratchNames, connection: &mut StreamConn) {
    let Ok(observation) = OBSERVATION.try_with(Clone::clone) else {
        return;
    };
    let before = catalog(connection, names, &[]).await;
    let relay =
        (observation.scenario == Scenario::RelayUnconfirmed).then_some(&observation.control);
    let survived = fault(
        observation.scenario,
        &observation.workload,
        &observation.control,
        relay,
        names,
        &[],
        connection,
    )
    .await;
    if observation.scenario == Scenario::WorkloadUnconfirmed {
        // This is the original control stream. The retained workload's SQL
        // still existed on it after resume, before exact control removal.
        observe_unavailable(connection).await;
    }
    *observation.created.lock().unwrap() = Some(Created {
        names: names.clone(),
        before,
        survived,
    });
}

fn recovery(result: &Result<(), ServerFailure>) -> Vec<String> {
    eprintln!("1288 actual public cleanup succeeded={}", result.is_ok());
    result
        .as_ref()
        .err()
        .map(|failure| failure.recovery_names.clone())
        .unwrap_or_default()
}

fn assert_reports(
    scenario: Scenario,
    sql: &[String],
    workload: &Owned,
    relay: Option<&Owned>,
    first: &[String],
    repeated: &[String],
) {
    for (label, report) in [("first", first), ("repeated", repeated)] {
        eprintln!("1288 {scenario:?}/{label} recovery={report:?}");
        for name in sql {
            assert_eq!(
                report.contains(name),
                scenario == Scenario::WorkloadUnconfirmed,
                "{scenario:?}/{label}: SQL recovery must follow confirmed workload removal: {name}"
            );
        }
        assert_eq!(
            report.contains(&workload.name),
            scenario == Scenario::WorkloadUnconfirmed,
            "{scenario:?}/{label}: retain the unconfirmed exact workload only"
        );
        if scenario == Scenario::RelayUnconfirmed {
            assert!(
                report.contains(&relay.unwrap().name),
                "{label}: independent relay obligation must remain"
            );
        }
    }
}

#[tokio::test]
#[ignore = "requires the owned native Docker daemon and a pinned PostgreSQL TLS target"]
async fn confirmed_workload_removal_finishes_only_its_existing_sql_recovery() {
    let (socket, image, version) = fixture();
    bounded("pinned target version", actual_target_version(version)).await;
    for scenario in [
        Scenario::Removed,
        Scenario::WorkloadUnconfirmed,
        Scenario::RelayUnconfirmed,
    ] {
        let mut case = Case::new();
        let mut target = bounded("native target admission", target()).await;
        let mut candidate = bounded(
            "native candidate admission",
            candidate(&mut target, &socket, image),
        )
        .await;
        case.record();
        let (workload, control) = case.startup();
        let recipe = bounded("target database recipe", target.database_recipe())
            .await
            .unwrap();
        let mut run = bounded("open scratch", candidate.open_scratch(&recipe))
            .await
            .unwrap();
        assert_eq!(
            bounded(
                "qualify scratch",
                run.qualify(&mut target, &ScopeRequest::default())
            )
            .await
            .unwrap(),
            Verdict::Verified
        );
        let roles = run.inner.roles_mut().clone();
        assert!(
            !roles.is_empty(),
            "normal qualification must create run-local roles"
        );
        assert!(
            roles.iter().any(|role| role != run.names.login()),
            "normal qualification must establish a principal beyond the scratch login"
        );
        let sql = sql_names(&run.names, &roles);
        let names = run.names.clone();
        let before = catalog(
            run.scratch.as_mut().unwrap().connection_mut(),
            &names,
            &roles,
        )
        .await;
        case.record();
        let relay = (scenario == Scenario::RelayUnconfirmed)
            .then(|| case.independent_relay(&workload, &control));
        let survived = fault(
            scenario,
            &workload,
            &control,
            relay.as_ref(),
            &names,
            &roles,
            run.scratch.as_mut().unwrap().connection_mut(),
        )
        .await;
        let first = recovery(&bounded("close scratch", run.close()).await);
        let repeated = recovery(&bounded("close scratch", run.close()).await);
        let workload_retained = workload.checked().is_some();
        let relay_retained = relay
            .as_ref()
            .is_some_and(|relay| relay.checked().is_some());
        let terminal = bounded("terminal scratch check", run.check(&mut target))
            .await
            .is_err()
            && bounded(
                "terminal scratch qualify",
                run.qualify(&mut target, &ScopeRequest::default()),
            )
            .await
            .is_err();
        case.cleanup();
        assert_eq!(
            before, sql,
            "actual catalog establishes normal qualification role provenance"
        );
        if scenario == Scenario::WorkloadUnconfirmed {
            assert_eq!(survived, sql, "same owned SQL survives pause and resume");
        }
        assert_reports(scenario, &sql, &workload, relay.as_ref(), &first, &repeated);
        assert_eq!(workload_retained, scenario == Scenario::WorkloadUnconfirmed);
        if scenario == Scenario::RelayUnconfirmed {
            assert!(relay_retained);
        }
        assert!(terminal, "resource cleanup cannot revive analysis");
        bounded("unrelated target check", target.check())
            .await
            .unwrap();
        bounded("pinned target version", actual_target_version(version)).await;
    }
}

#[tokio::test]
#[ignore = "requires the owned native Docker daemon and a pinned PostgreSQL TLS target"]
async fn interrupted_open_discard_finishes_sql_recovery_only_after_workload_removal() {
    let (socket, image, version) = fixture();
    bounded("pinned target version", actual_target_version(version)).await;
    for scenario in [
        Scenario::Removed,
        Scenario::WorkloadUnconfirmed,
        Scenario::RelayUnconfirmed,
    ] {
        let mut case = Case::new();
        let mut target = bounded("native target admission", target()).await;
        let mut candidate = bounded(
            "native candidate admission",
            candidate(&mut target, &socket, image),
        )
        .await;
        case.record();
        let (workload, control) = case.startup();
        let recipe = bounded("target database recipe", target.database_recipe())
            .await
            .unwrap();
        let created = Arc::new(Mutex::new(None));
        let observation = Observation {
            scenario,
            workload: workload.clone(),
            control: control.clone(),
            created: created.clone(),
        };
        OBSERVATION
            .scope(
                observation,
                cancel_after("container-create-owned", candidate.open_scratch(&recipe)),
            )
            .await;
        let created = created
            .lock()
            .unwrap()
            .take()
            .expect("the approved observer records real scratch creation before cancellation");
        let sql = sql_names(&created.names, &[]);
        let first = recovery(&bounded("discard candidate", candidate.discard()).await);
        let repeated = recovery(&bounded("discard candidate", candidate.discard()).await);
        let workload_retained = workload.checked().is_some();
        let relay_retained = control.checked().is_some();
        let terminal = candidate.identity().is_err()
            && bounded("terminal candidate check", candidate.check())
                .await
                .is_err()
            && bounded("terminal scratch reopen", candidate.open_scratch(&recipe))
                .await
                .is_err();
        case.cleanup();
        assert_eq!(
            created.before, sql,
            "interrupted open created the exact database and login"
        );
        if scenario == Scenario::WorkloadUnconfirmed {
            assert_eq!(created.survived, sql);
        }
        assert_reports(scenario, &sql, &workload, Some(&control), &first, &repeated);
        assert_eq!(workload_retained, scenario == Scenario::WorkloadUnconfirmed);
        if scenario == Scenario::RelayUnconfirmed {
            assert!(relay_retained);
        }
        assert!(terminal, "discard cannot revive a cancelled open");
        bounded("unrelated target check", target.check())
            .await
            .unwrap();
        bounded("pinned target version", actual_target_version(version)).await;
    }
}

#[tokio::test]
#[ignore = "requires the owned native Docker daemon and a pinned PostgreSQL TLS target"]
async fn cancelled_cleanup_keeps_uncertain_owned_sql_names_and_remains_terminal() {
    let (socket, image, version) = fixture();
    bounded("pinned target version", actual_target_version(version)).await;
    for site in ["close", "discard"] {
        let mut case = Case::new();
        let mut target = bounded("native target admission", target()).await;
        let mut candidate = bounded(
            "native candidate admission",
            candidate(&mut target, &socket, image),
        )
        .await;
        case.record();
        let (workload, control) = case.startup();
        let recipe = bounded("target database recipe", target.database_recipe())
            .await
            .unwrap();
        let (sql, before, survived, first, repeated, terminal) = if site == "close" {
            let mut run = bounded("open scratch", candidate.open_scratch(&recipe))
                .await
                .unwrap();
            assert_eq!(
                bounded(
                    "qualify scratch",
                    run.qualify(&mut target, &ScopeRequest::default())
                )
                .await
                .unwrap(),
                Verdict::Verified
            );
            let roles = run.inner.roles_mut().clone();
            assert!(!roles.is_empty());
            assert!(roles.iter().any(|role| role != run.names.login()));
            let names = run.names.clone();
            let sql = sql_names(&names, &roles);
            let before = catalog(
                run.scratch.as_mut().unwrap().connection_mut(),
                &names,
                &roles,
            )
            .await;
            case.record();
            let survived = fault(
                Scenario::WorkloadUnconfirmed,
                &workload,
                &control,
                None,
                &names,
                &roles,
                run.scratch.as_mut().unwrap().connection_mut(),
            )
            .await;
            cancel_after("container-close-owned", run.close()).await;
            let terminal = bounded("terminal scratch check", run.check(&mut target))
                .await
                .is_err()
                && bounded(
                    "terminal scratch qualify",
                    run.qualify(&mut target, &ScopeRequest::default()),
                )
                .await
                .is_err();
            (
                sql,
                before,
                survived,
                recovery(&bounded("close scratch", run.close()).await),
                recovery(&bounded("close scratch", run.close()).await),
                terminal,
            )
        } else {
            let created = Arc::new(Mutex::new(None));
            let observation = Observation {
                scenario: Scenario::WorkloadUnconfirmed,
                workload: workload.clone(),
                control: control.clone(),
                created: created.clone(),
            };
            OBSERVATION
                .scope(
                    observation,
                    cancel_after("container-create-owned", candidate.open_scratch(&recipe)),
                )
                .await;
            let created = created.lock().unwrap().take().unwrap();
            cancel_after("container-discard-owned", candidate.discard()).await;
            let terminal = candidate.identity().is_err()
                && bounded("terminal candidate check", candidate.check())
                    .await
                    .is_err()
                && bounded("terminal scratch reopen", candidate.open_scratch(&recipe))
                    .await
                    .is_err();
            (
                sql_names(&created.names, &[]),
                created.before,
                created.survived,
                recovery(&bounded("discard candidate", candidate.discard()).await),
                recovery(&bounded("discard candidate", candidate.discard()).await),
                terminal,
            )
        };
        let retained = workload.checked().is_some();
        case.cleanup();
        assert_eq!(before, sql);
        assert_eq!(survived, sql);
        assert!(
            retained,
            "a real ownership refusal leaves the exact workload observable"
        );
        assert_reports(
            Scenario::WorkloadUnconfirmed,
            &sql,
            &workload,
            None,
            &first,
            &repeated,
        );
        assert!(
            terminal,
            "{site}: a cancelled cleanup cannot resume analysis"
        );
        bounded("unrelated target check", target.check())
            .await
            .unwrap();
        bounded("pinned target version", actual_target_version(version)).await;
    }
}
