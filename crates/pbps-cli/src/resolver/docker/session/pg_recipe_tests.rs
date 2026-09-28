//! Independent PG16/PG18 container recipe oracles for #1279.
//!
//! Image metadata selects only the fixed filesystem layout. The native case
//! separately observes the running engine, kernel mounts and exact cleanup.

use super::*;
use crate::resolver::docker::profile::Launch;
use crate::resolver::native::ProcessLease;
use pbps_config::resolver::{PullPolicy, ResolverProfile};
use pbps_db::transport::PeerVerifiedConn;
use serde_json::{Value, json};
use std::path::Path;

const PG16_IMAGE: &str =
    "postgres@sha256:485935f94cc7165afa896978809c37b592dc07f0a37d2c8f645f12412d0212c8";
const PG18_IMAGE: &str =
    "postgres@sha256:4ef4dbc939d61acea57712655ddb4b4ab27419c913f94cca0cd57cb3ea3c2280";
const PG16_ROOT: &str = "/var/lib/postgresql/data";
const PG18_ROOT: &str = "/var/lib/postgresql";

fn candidate(config: Value) -> Result<CandidateImage, String> {
    let raw: super::super::ImageInspect = serde_json::from_value(json!({
        "Id": format!("sha256:{}", "a".repeat(64)),
        "Os": "linux",
        "Architecture": "amd64",
        "Config": config,
    }))
    .map_err(|error| error.to_string())?;
    candidate_from_raw(raw)
}

fn candidate_from_raw(raw: super::super::ImageInspect) -> Result<CandidateImage, String> {
    raw.try_into().map_err(|error: Error| error.to_string())
}

fn launch(config: Value) -> Result<Launch, String> {
    let image = candidate(config)?;
    Launch::reserved(&image, Driver::Postgres, "owned-pg-layout", "test-password")
        .map_err(|error| error.to_string())
}

fn config(volumes: Option<Value>) -> Value {
    let mut config = json!({"Env": ["PATH=/image/bin", "PG_MAJOR=untrusted"]});
    if let Some(volumes) = volumes {
        config["Volumes"] = volumes;
    }
    config
}

fn assert_layout(launch: &Launch, root: &str, major: u32) {
    let storage = &launch.body["HostConfig"]["Tmpfs"];
    assert_eq!(
        storage[root],
        "rw,nosuid,nodev,noexec,size=268435456,uid=999,gid=999,mode=700"
    );
    let other = if major == 16 { PG18_ROOT } else { PG16_ROOT };
    assert!(
        storage.get(other).is_none(),
        "only the selected data root is writable"
    );
    let command = launch.body["Cmd"]
        .as_array()
        .unwrap()
        .last()
        .unwrap()
        .as_str()
        .unwrap();
    assert!(
        command.contains(&format!("/{major}/bin/initdb")),
        "{command}"
    );
    assert!(
        command.contains(&format!("/{major}/bin/postgres")),
        "{command}"
    );
    assert!(command.contains(&format!("{root}/run-data")), "{command}");
    assert!(!command.contains(&format!("/{}/bin/", if major == 16 { 18 } else { 16 })));
}

#[test]
fn a_declared_pg16_data_volume_selects_only_the_measured_private_layout() {
    // Deliberately contradict PG_MAJOR: this inherited environment value is
    // neither a layout selector nor engine-version evidence.
    let selected = launch(config(Some(json!({"/var/lib/postgresql/data": {}})))).unwrap();
    assert_layout(&selected, PG16_ROOT, 16);
    let env = selected.body["Env"].as_array().unwrap();
    assert!(env.contains(&json!("PG_MAJOR")));
    assert!(
        !env.iter()
            .any(|entry| entry.as_str() == Some("PG_MAJOR=untrusted"))
    );
}

#[test]
fn no_declared_volume_or_the_exact_parent_root_keeps_the_pg18_layout() {
    // An Env-known synthetic image with no Volumes field already qualified
    // before #1279. Missing, null and empty all mean no declared volume here.
    for volumes in [
        None,
        Some(Value::Null),
        Some(json!({})),
        Some(json!({"/var/lib/postgresql": {}})),
    ] {
        let selected = launch(config(volumes.clone())).unwrap();
        assert_layout(&selected, PG18_ROOT, 18);
    }
}

#[test]
fn control_and_workload_use_the_same_selected_private_storage() {
    for (volumes, root) in [
        (None, PG18_ROOT),
        (Some(json!({"/var/lib/postgresql/data": {}})), PG16_ROOT),
    ] {
        let image = candidate(config(volumes)).unwrap();
        let workload = Launch::reserved(&image, Driver::Postgres, "owner", "password").unwrap();
        let control = Launch::control(
            &image,
            Driver::Postgres,
            "owner",
            &"a".repeat(64),
            super::super::profile::LIFETIME_SECS,
        )
        .unwrap();
        assert_eq!(
            control.body["HostConfig"]["Tmpfs"][root],
            workload.body["HostConfig"]["Tmpfs"][root]
        );
        let other = if root == PG16_ROOT {
            PG18_ROOT
        } else {
            PG16_ROOT
        };
        assert!(control.body["HostConfig"]["Tmpfs"].get(other).is_none());
    }
}

#[test]
fn malformed_ambiguous_or_unexpected_image_storage_cannot_select_a_recipe() {
    for volumes in [
        json!({"/var/lib/postgresql/data": {}, "/var/lib/postgresql": {}}),
        json!({"/var/lib/postgresql/data": {}, "/other": {}}),
        json!({"/unexpected-disk": {}}),
        json!([PG16_ROOT]),
        json!(PG16_ROOT),
        json!({"/var/lib/postgresql/data": "not a volume declaration"}),
    ] {
        assert!(launch(config(Some(volumes.clone()))).is_err(), "{volumes}");
    }
    assert!(
        launch(Value::Null).is_err(),
        "null Config is unknown metadata"
    );
    let absent: Result<super::super::ImageInspect, _> = serde_json::from_value(json!({
        "Id": format!("sha256:{}", "a".repeat(64)),
        "Os": "linux", "Architecture": "amd64"
    }));
    assert!(
        absent
            .map_err(|error| error.to_string())
            .and_then(|raw| candidate_from_raw(raw))
            .and_then(
                |image| Launch::reserved(&image, Driver::Postgres, "owner", "password")
                    .map_err(|error| error.to_string())
            )
            .is_err(),
        "missing Config must not select the default layout"
    );
    assert!(launch(json!({"Env": "LD_PRELOAD=/outside"})).is_err());
}

fn observed(launch: &Launch) -> Value {
    let mut observed = json!({
        "Config": launch.body.clone(),
        "HostConfig": launch.body["HostConfig"].clone(),
        "Mounts": []
    });
    observed["HostConfig"]["Privileged"] = json!(false);
    observed["HostConfig"]["PublishAllPorts"] = json!(false);
    for key in ["PidMode", "IpcMode", "UTSMode", "UsernsMode"] {
        observed["HostConfig"][key] = json!("");
    }
    observed
}

#[test]
fn inherited_disk_mounts_and_loader_hooks_remain_refused_for_both_layouts() {
    for volumes in [None, Some(json!({"/var/lib/postgresql/data": {}}))] {
        let mut image_config = config(volumes);
        image_config["Env"] = json!([
            "PATH=/image/bin",
            "PG_MAJOR=untrusted",
            "PGDATA=/outside",
            "BASH_ENV=/outside/startup",
            "ENV=/outside/startup",
            "LD_PRELOAD=/outside/lib.so",
            "LD_LIBRARY_PATH=/outside"
        ]);
        let selected = launch(image_config).unwrap();
        let clean = observed(&selected);
        assert!(selected.check_configuration(&clean).is_ok());
        let env = selected.body["Env"].as_array().unwrap();
        for key in ["BASH_ENV", "ENV", "LD_PRELOAD", "LD_LIBRARY_PATH", "PGDATA"] {
            assert!(
                env.contains(&json!(key)),
                "{key} must be removed before startup"
            );
            let mut injected = clean.clone();
            injected["Config"]["Env"]
                .as_array_mut()
                .unwrap()
                .push(json!(format!("{key}=/outside")));
            assert!(selected.check_configuration(&injected).is_err(), "{key}");
        }
        for extra in [
            json!({"Type":"volume", "Destination":PG16_ROOT}),
            json!({"Type":"bind", "Destination":"/unexpected-disk"}),
            json!({"Type":"tmpfs", "Destination":"/unexpected-disk"}),
        ] {
            let mut injected = clean.clone();
            injected["Mounts"] = json!([extra]);
            assert!(
                selected.check_configuration(&injected).is_err(),
                "{injected}"
            );
        }
    }
}

fn selected_major() -> (u32, &'static str, &'static str, u32) {
    match std::env::var("PBPS_NATIVE_PG_MAJOR").unwrap().as_str() {
        "16" => (16, PG16_IMAGE, PG16_ROOT, 160015),
        "18" => (18, PG18_IMAGE, PG18_ROOT, 180006),
        other => panic!("unsupported pinned PostgreSQL fixture {other}"),
    }
}

fn descendants(root: u32) -> Vec<u32> {
    let mut pending = vec![root];
    let mut seen = Vec::new();
    while let Some(pid) = pending.pop() {
        assert!(
            !seen.contains(&pid),
            "a process cannot be its own descendant"
        );
        seen.push(pid);
        assert!(seen.len() <= 512, "the owned process tree must be bounded");
        // A transient bootstrap child may exit between the kernel child list
        // and capture; the persistent postmaster still has to be observed.
        let Ok(process) = ProcessLease::capture(pid) else {
            assert_ne!(pid, root, "the owned process root disappeared");
            continue;
        };
        let Ok(children) = process.read_proc(&format!("task/{pid}/children"), 4096) else {
            assert_ne!(pid, root, "the owned process root disappeared");
            continue;
        };
        pending.extend(
            children
                .split_whitespace()
                .map(|child| child.parse::<u32>().unwrap()),
        );
    }
    seen
}

#[tokio::test]
#[ignore = "requires the owned native Docker daemon and a pinned PostgreSQL TLS target"]
async fn the_public_factory_runs_the_pinned_engine_on_bounded_private_storage() {
    assert_eq!(
        std::env::var("PBPS_NATIVE_FACTORY_FIXTURE").as_deref(),
        Ok("1")
    );
    assert_eq!(std::env::var("PBPS_NATIVE_DRIVER").as_deref(), Ok("pg"));
    let (major, image_ref, storage_root, version_num) = selected_major();
    assert_eq!(
        std::env::var("PBPS_RESOLVER_TEST_IMAGE").as_deref(),
        Ok(image_ref)
    );
    let socket = std::env::var("PBPS_RESOLVER_TEST_SOCKET").unwrap();
    let peer = PeerVerifiedConn::connect(
        Driver::Postgres,
        &std::env::var("PBPS_NATIVE_CONNECTION").unwrap(),
    )
    .await
    .unwrap();
    let service = std::env::var("PBPS_NATIVE_SERVICE_PID")
        .unwrap()
        .parse()
        .unwrap();
    let mut target = NativeTarget::establish(peer, service).await.unwrap();
    let mut api = LocalApi::connect_native(Path::new(&socket)).await.unwrap();
    let image = api
        .acquire(&ResolverProfile::Docker {
            image: image_ref.into(),
            pull: PullPolicy::Never,
        })
        .await
        .unwrap();
    let mut session = CandidateSession::start(api, image, &mut target)
        .await
        .expect("the public native factory must qualify the selected layout");
    session.verify_target_separation(&mut target).await.unwrap();
    session.check().await.unwrap();
    let state = session.state.as_mut().unwrap();
    let resources = [
        (
            state.control.container_id().to_owned(),
            state.control.resource_name().to_owned(),
        ),
        (
            state.workload.container_id().to_owned(),
            state.workload.resource_name().to_owned(),
        ),
    ];
    let rows = state.connection.query(
        "SELECT current_setting('server_version_num') AS version_num, current_setting('data_directory') AS data_directory, pg_catalog.version() AS engine"
    ).await.unwrap();
    let reported_version: u32 = rows[0]
        .try_get::<&str>("version_num")
        .unwrap()
        .unwrap()
        .parse()
        .unwrap();
    let reported_directory = rows[0]
        .try_get::<&str>("data_directory")
        .unwrap()
        .unwrap()
        .to_owned();
    let reported_engine = rows[0]
        .try_get::<&str>("engine")
        .unwrap()
        .unwrap()
        .to_owned();
    let root_pid = state.workload.native_pid().unwrap();
    let process = ProcessLease::capture(root_pid).unwrap();
    let mountinfo = process.read_proc("mountinfo", 262144).unwrap();
    let selected_mounts: Vec<_> = mountinfo
        .lines()
        .filter(|line| line.split_whitespace().nth(4) == Some(storage_root))
        .collect();
    let selected_mount = selected_mounts.first().copied().unwrap_or("").to_owned();
    let executable = format!("/usr/lib/postgresql/{major}/bin/postgres");
    let engine_paths: Vec<_> = descendants(root_pid)
        .into_iter()
        .filter_map(|pid| {
            ProcessLease::capture(pid)
                .ok()
                .map(|process| process.executable_path().to_path_buf())
        })
        .collect();
    let mut observer = LocalApi::connect_native(Path::new(&socket)).await.unwrap();
    let mut configurations = Vec::new();
    for (id, _) in &resources {
        configurations.push(observer.inspect_container(id).await.unwrap().unwrap());
    }
    let closed = session.close().await;
    let removed = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            let mut remaining = Vec::new();
            for (id, name) in &resources {
                if observer.inspect_container(id).await.unwrap().is_some() {
                    remaining.push(name.clone());
                }
            }
            if remaining.is_empty() || closed.is_err() {
                break remaining;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("owned container removal must settle");
    if let Err(failure) = &closed {
        for name in &removed {
            assert!(
                failure.recovery_names.contains(name),
                "unconfirmed owned resource {name} was unnamed"
            );
        }
    }
    assert!(
        closed.is_ok(),
        "the ordinary run must cleanly close: {closed:?}"
    );
    assert!(removed.is_empty(), "owned containers remained: {removed:?}");
    assert_eq!(
        reported_version, version_num,
        "actual engine: {reported_engine}"
    );
    assert_eq!(reported_directory, format!("{storage_root}/run-data"));
    assert_eq!(selected_mounts.len(), 1, "selected mount: {selected_mount}");
    assert!(selected_mount.contains(" - tmpfs "), "{selected_mount}");
    for flag in ["rw", "nosuid", "nodev", "noexec"] {
        assert!(
            selected_mount
                .split_whitespace()
                .nth(5)
                .unwrap()
                .split(',')
                .any(|item| item == flag),
            "{selected_mount}"
        );
    }
    assert!(selected_mount.contains("size=262144k"), "{selected_mount}");
    assert!(
        engine_paths
            .iter()
            .any(|path| path.ends_with(Path::new(&executable))),
        "actual executable paths: {engine_paths:?}"
    );
    for configuration in &configurations {
        assert!(
            configuration["Mounts"]
                .as_array()
                .unwrap()
                .iter()
                .all(|mount| mount["Type"] == "tmpfs"),
            "image-declared disk volume survived: {configuration}"
        );
        assert_eq!(
            configuration["HostConfig"]["Tmpfs"][storage_root],
            "rw,nosuid,nodev,noexec,size=268435456,uid=999,gid=999,mode=700"
        );
    }
    target.check().await.unwrap();
}
