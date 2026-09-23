//! SQL reads expose all runtime-file bytes, including otherwise ignored comments.

use super::*;
use crate::resolver::docker::file_fixture::{ChangedFile, mutations};
use crate::resolver::native::{FORWARDER_PRIVILEGES, ProcessLease, guarded_tasks};

#[tokio::test]
#[ignore = "requires root and explicitly owned Docker engine fixtures"]
async fn host_information_cannot_enter_either_private_runtime_view() {
    let path = PathBuf::from(std::env::var("PBPS_RESOLVER_TEST_SOCKET").unwrap());
    let reference = std::env::var("PBPS_RESOLVER_TEST_IMAGE").unwrap();
    let driver = match std::env::var("PBPS_RESOLVER_TEST_DRIVER").unwrap().as_str() {
        "pg" => Driver::Postgres,
        "mssql" => Driver::Mssql,
        _ => panic!("explicit supported engine required"),
    };
    let mut api = LocalApi::connect(&path).await.unwrap();
    let image = api
        .inspect_image(&reference)
        .await
        .unwrap()
        .expect("no fixture pull");
    // Test-only source-free channels let us observe what SQL can read even
    // when the real native premise has refused the mutated runtime.
    let mut session = CandidateSession::start_channels(
        LocalApi::connect(&path).await.unwrap(),
        LocalApi::connect(&path).await.unwrap(),
        LocalApi::connect(&path).await.unwrap(),
        image,
        driver,
    )
    .await
    .unwrap();
    let observed = async {
        let state = session.state.as_mut().unwrap();
        let process = ProcessLease::capture(state.workload.native_pid().unwrap()).map_err(|_| "initial process capture".to_owned())?;
        crate::resolver::native::runtime_files::check(&process).map_err(|_| "initial runtime files".to_owned())?;
        let workload = ExecutionLease::capture(state.workload.native_pid().unwrap(), engine::workload_limits(driver)).map_err(|_| "initial workload execution".to_owned())?;
        let control = ExecutionLease::capture(state.control.native_pid().unwrap(), engine::control_limits(driver)).map_err(|_| "initial control execution".to_owned())?;
        let guard = ProcessLease::capture(state.control.native_pid().unwrap()).map_err(|_| "initial forwarder guard".to_owned())?;
        guarded_tasks(&guard, FORWARDER_PRIVILEGES).map_err(|_| "forwarder guard check".to_owned())?;
        let mut accepted = Vec::new();
        for path in ["/etc/resolv.conf", "/etc/hosts", "/etc/hostname"] {
            let changed = ChangedFile::capture(&mut api, state.workload.container_id(), path).await;
            let control_file = ChangedFile::capture(&mut api, state.control.container_id(), path).await;
            for (mode, text) in mutations(path, changed.original()) {
                changed.replace(&text);
                control_file.replace(&text);
                state.workload.check().await.unwrap();
                state.control.check().await.unwrap();
                let workload_accepts = workload.check().is_ok();
                let control_accepts = control.check().is_ok();
                let guard_accepts = guarded_tasks(&guard, FORWARDER_PRIVILEGES).is_ok();
                let sql = match driver {
                    Driver::Postgres => format!("SELECT pg_read_file('{path}') AS contents"),
                    Driver::Mssql => format!("SELECT BulkColumn AS contents FROM OPENROWSET(BULK '{path}', SINGLE_CLOB) AS f"),
                };
                let rows = state.connection.query(&sql).await.map_err(|error| error.to_string())?;
                let actual = rows[0].try_get::<&str>("contents").map_err(|error| error.to_string())?.unwrap();
                assert_eq!(actual, text, "complete engine-side read for {mode}");
                eprintln!("host files mode={mode}, workload={workload_accepts}, control={control_accepts}, guard={guard_accepts}, complete_sql_read=true");
                if workload_accepts || control_accepts || guard_accepts {
                    accepted.push(mode.to_owned());
                }
                control_file.restore();
                changed.restore();
                workload.check().unwrap();
                control.check().unwrap();
                guarded_tasks(&guard, FORWARDER_PRIVILEGES).map_err(|_| "forwarder guard check".to_owned())?;
            }
        }
        state.connection.execute("CREATE TABLE pbps_host_file_table (id integer)").await.map_err(|error| error.to_string())?;
        state.connection.execute("CREATE VIEW pbps_host_file_view AS SELECT id FROM pbps_host_file_table").await.map_err(|error| error.to_string())?;
        assert_eq!(state.connection.query("SELECT COUNT(*) FROM pbps_host_file_view").await.map_err(|error| error.to_string())?.len(), 1);
        Ok::<_, String>(accepted)
    }.await;
    session
        .close()
        .await
        .expect("ordinary cleanup must remove both owned containers");
    let accepted = observed.unwrap();
    assert!(
        accepted.is_empty(),
        "host information was admitted: {accepted:?}"
    );
}
