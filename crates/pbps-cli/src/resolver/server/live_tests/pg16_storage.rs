use super::*;
use crate::resolver::native::mount_rows;
use serde_json::{Value, json};
use std::os::unix::fs::MetadataExt as _;

fn text(row: &pbps_db::Row, column: &str) -> Result<String, String> {
    row.try_get::<&str>(column)
        .map_err(|error| error.to_string())?
        .map(str::to_owned)
        .ok_or_else(|| format!("{column} is null"))
}

async fn named_object(
    connection: &mut StreamConn,
    database: bool,
    name: &str,
) -> Result<bool, String> {
    let (catalog, column) = if database {
        ("pg_database", "datname")
    } else {
        ("pg_roles", "rolname")
    };
    let quoted = name.replace('\'', "''");
    let rows = connection
        .query(&format!(
            "SELECT count(*)::pg_catalog.text AS n FROM pg_catalog.{catalog} WHERE {column} = '{quoted}'"
        ))
        .await
        .map_err(|error| error.to_string())?;
    if rows.len() != 1 {
        return Err("the exact-name catalog query did not return one row".into());
    }
    match text(&rows[0], "n")?.as_str() {
        "0" => Ok(false),
        "1" => Ok(true),
        other => Err(format!(
            "an exact catalog name has unexpected count {other}"
        )),
    }
}

async fn observation(server: &mut DedicatedServer, marker: &str) -> Result<Value, String> {
    server.check().await.map_err(|error| error.to_string())?;
    server.identity().map_err(|error| error.to_string())?;
    let inner = server.inner.as_mut().ok_or("admission was consumed")?;
    let control = &mut inner.control;
    let record = control
        .api
        .inspect_container(&control.pinned.id)
        .await
        .map_err(|error| error.to_string())?
        .ok_or("the supplied container disappeared")?;
    if pin(&record).map_err(|error| error.to_string())? != control.pinned {
        return Err("the supplied identity changed while observing its layout".into());
    }
    let session = control
        .session
        .as_mut()
        .ok_or("the control session is gone")?;
    let reported = session
        .connection
        .query(
            "SELECT pg_catalog.current_setting('server_version_num')::pg_catalog.text AS version, \
         pg_catalog.current_setting('data_directory')::pg_catalog.text AS directory",
        )
        .await
        .map_err(|error| error.to_string())?;
    if reported.len() != 1 {
        return Err("the actual engine did not return one version/directory row".into());
    }
    let version = text(&reported[0], "version")?
        .parse::<u32>()
        .map_err(|error| error.to_string())?;
    let directory = text(&reported[0], "directory")?;
    let marker_present = named_object(&mut session.connection, true, marker).await?;
    let runtime = &inner
        .analysis
        .as_ref()
        .ok_or("analysis was invalidated")?
        .runtime;
    let rows = mount_rows(runtime.init()).map_err(|error| error.to_string())?;
    profile::contained(&rows, control.profile)?;
    let storage = std::fs::metadata(format!(
        "/proc/{}/root{}",
        control.pinned.pid, control.profile.storage_path
    ))
    .map_err(|error| error.to_string())?;
    runtime.init().check().map_err(|error| error.to_string())?;
    let facts = json!({
        "profile": control.endpoint.profile,
        "container_id": control.pinned.id,
        "image_id": control.pinned.image,
        "container_name": record["Name"],
        "owner_labels": record["Config"]["Labels"],
        "configured_user": record["Config"]["User"],
        "daemon_state": record["State"],
        "host_config": record["HostConfig"],
        "daemon_mounts": record["Mounts"],
        "engine_executable": runtime.engine().executable_path(),
        "version_num": version,
        "data_directory": directory,
        "marker_present": marker_present,
        "storage_uid": storage.uid(), "storage_gid": storage.gid(),
        "storage_mode": storage.mode() & 0o7777,
        "kernel_rows": rows.iter().map(|row| json!({
            "target": row.target, "kind": row.kind, "device": row.device,
            "root": row.root, "options": row.options
        })).collect::<Vec<_>>()
    });
    server.check().await.map_err(|error| error.to_string())?;
    Ok(facts)
}

#[tokio::test]
#[ignore = "requires a disposable native Linux host and the dedicated-server fixtures"]
async fn the_supplied_storage_layout_admits_its_observed_major_and_survives_live_checks() {
    fixture();
    assert_eq!(
        driver(),
        Driver::Postgres,
        "this exact fixture case is PostgreSQL-only"
    );
    let configured = endpoint("PBPS_SERVER_ENDPOINT");
    let expected_major: u64 = std::env::var("PBPS_SERVER_PG_MAJOR")
        .unwrap()
        .parse()
        .unwrap();
    let (expected_profile, expected_storage) = match expected_major {
        16 => ("linux-dedicated-pg16-v1", "/var/lib/postgresql/data"),
        18 => ("linux-dedicated-v1", "/var/lib/postgresql"),
        other => panic!("the fixed PG16/18 fixture selected unsupported major {other}"),
    };
    let marker = std::env::var("PBPS_SERVER_MARKER_DATABASE").unwrap();
    let mut target = native_target().await;
    let recipe = scratch_recipe(&mut target).await;
    let mut refusals = Vec::new();
    let (mut admitted, observed, mut run) = loop {
        let mut server = admit_when_exclusive("PBPS_SERVER_ENDPOINT", &mut target).await;
        let observed = match observation(&mut server, &marker).await {
            Ok(observed) => observed,
            Err(_) => {
                require_discarded(
                    FixtureFailure::AdministrativeObservation,
                    server.discard().await,
                );
                panic!(
                    "the qualified administrative observation failed: {}",
                    FixtureFailure::AdministrativeObservation
                );
            }
        };
        match server.open_scratch(&recipe).await {
            Ok(run) => break (server, observed, run),
            Err(refused) => {
                require_discarded(FixtureFailure::Open(&refused), server.discard().await);
                match refused {
                    ServerFailure {
                        cause:
                            cause @ (Error::Exclusivity(_)
                            | Error::Containment(super::super::Premise::Occupants)),
                        recovery_names,
                    } if recovery_names.is_empty() && refusals.len() < 60 => refusals.push(cause),
                    other => {
                        panic!(
                            "the measured supplied layout must open a scratch run: {}",
                            SafeFailure(&other)
                        )
                    }
                }
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            }
        }
    };
    println!(
        "PBPS1302 {}",
        json!({"phase": "admitted", "facts": observed})
    );
    let consumed = matches!(admitted.identity(), Err(Error::Consumed));
    admitted
        .discard()
        .await
        .expect("the moved server has no competing cleanup owner");
    admitted
        .discard()
        .await
        .expect("repeated discard of the moved owner is harmless");
    let names = run.names.clone();
    let compiled = async {
        run.check(&mut target).await.map_err(|error| error.to_string())?;
        let connection = &mut run.scratch.as_mut().ok_or("scratch session is gone")?.connection;
        let rows = connection.query(
            "SELECT pg_catalog.current_database()::pg_catalog.text AS name, current_user::pg_catalog.text AS login"
        ).await.map_err(|error| error.to_string())?;
        if rows.len() != 1 { return Err("the scratch session did not return one identity row".into()); }
        let database = text(&rows[0], "name")?;
        let login = text(&rows[0], "login")?;
        connection.execute("CREATE TABLE pbps_profile_probe (id integer)").await
            .map_err(|error| error.to_string())?;
        connection.execute("CREATE VIEW pbps_profile_view AS SELECT id FROM pbps_profile_probe").await
            .map_err(|error| error.to_string())?;
        run.check(&mut target).await.map_err(|error| error.to_string())?;
        run.check(&mut target).await.map_err(|error| error.to_string())?;
        Ok::<_, String>((database, login))
    }.await;
    let first_close = run.close().await;
    let repeated_close = run.close().await;
    let terminal = matches!(run.check(&mut target).await, Err(Error::Consumed));
    let mut admin = session(&configured, maintenance()).await;
    let database_present = named_object(&mut admin.connection, true, names.database()).await;
    let login_present = named_object(&mut admin.connection, false, names.login()).await;
    let marker_present = named_object(&mut admin.connection, true, &marker).await;
    admin.close().await;
    let mut api = LocalApi::connect_native(&configured.daemon).await.unwrap();
    let after = api
        .inspect_container(&configured.container)
        .await
        .unwrap()
        .unwrap();
    let target_usable = target.check().await;
    println!(
        "PBPS1302 {}",
        json!({
            "phase": "cleanup", "database": names.database(), "login": names.login(),
            "database_present": database_present, "login_present": login_present,
            "marker_present": marker_present, "container_id": after["Id"],
            "container_image": after["Image"], "container_state": after["State"],
            "product_first_close_ok": first_close.is_ok(),
            "product_repeated_close_ok": repeated_close.is_ok(), "terminal": terminal
        })
    );
    // Contract assertions follow product cleanup and the inspection forwarder's
    // confirmed close. A bad observation is never repaired by fixture deletion.
    first_close.expect("the product confirms exact scratch database/login removal");
    repeated_close.expect("completed cleanup remains idempotent");
    let (database, login) =
        compiled.expect("the actual scratch connection compiles and passes repeated live checks");
    assert_eq!(database, names.database());
    assert_eq!(login, names.login());
    assert!(
        consumed && terminal,
        "moved and closed owners cannot resume analysis"
    );
    assert_eq!(configured.profile, expected_profile);
    assert_eq!(observed["profile"], expected_profile);
    assert_eq!(
        observed["version_num"].as_u64().unwrap() / 10000,
        expected_major
    );
    assert_eq!(
        observed["data_directory"],
        format!("{expected_storage}/run-data")
    );
    assert_eq!(observed["storage_uid"], 999);
    assert_eq!(observed["storage_gid"], 999);
    assert_eq!(observed["storage_mode"], 0o700);
    let rows = observed["kernel_rows"].as_array().unwrap();
    let storage: Vec<_> = rows
        .iter()
        .filter(|row| row["target"] == expected_storage)
        .collect();
    assert_eq!(storage.len(), 1, "one exact storage row is required");
    assert_eq!(storage[0]["kind"], "tmpfs");
    assert_eq!(storage[0]["root"], "/");
    for flag in ["rw", "nosuid", "nodev", "noexec"] {
        assert!(
            storage[0]["options"]
                .as_array()
                .unwrap()
                .iter()
                .any(|value| value == flag),
            "storage requires {flag}"
        );
    }
    assert_eq!(observed["host_config"]["ReadonlyRootfs"], true);
    assert!(
        observed["daemon_mounts"]
            .as_array()
            .unwrap()
            .iter()
            .all(|mount| mount["Type"] == "tmpfs")
    );
    assert_eq!(
        observed["owner_labels"]["io.pbps.resolver.fixture"],
        "dedicated-server"
    );
    assert!(
        !database_present.unwrap(),
        "the exact generated database must be absent"
    );
    assert!(
        !login_present.unwrap(),
        "the exact generated login must be absent"
    );
    assert_eq!(observed["marker_present"], true);
    assert!(
        marker_present.unwrap(),
        "the pre-existing marker must survive"
    );
    assert_eq!(after["Id"], observed["container_id"]);
    assert_eq!(after["Image"], observed["image_id"]);
    assert_eq!(after["State"]["Pid"], observed["daemon_state"]["Pid"]);
    assert_eq!(
        after["State"]["StartedAt"],
        observed["daemon_state"]["StartedAt"]
    );
    assert_eq!(
        after["State"]["Running"], true,
        "the supplied engine must remain running"
    );
    target_usable.expect("the target remains usable after scratch cleanup");
}
