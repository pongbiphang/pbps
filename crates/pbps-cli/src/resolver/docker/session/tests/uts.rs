//! Actual UTS changes must invalidate each independent private runtime view.

use super::*;
use crate::resolver::docker::uts_fixture::ChangedUts;
use crate::resolver::native::{FORWARDER_PRIVILEGES, ProcessLease, guarded_tasks};

#[tokio::test]
#[ignore = "requires root and explicitly owned Docker engine fixtures"]
async fn kernel_names_cannot_enter_either_private_runtime_view() {
    let path = PathBuf::from(std::env::var("PBPS_RESOLVER_TEST_SOCKET").unwrap());
    let reference = std::env::var("PBPS_RESOLVER_TEST_IMAGE").unwrap();
    let driver = match std::env::var("PBPS_RESOLVER_TEST_DRIVER").unwrap().as_str() {
        "pg" => Driver::Postgres,
        "mssql" => Driver::Mssql,
        _ => panic!("explicit supported engine required"),
    };
    let mut api = LocalApi::connect(&path).await.unwrap();
    let image = api.inspect_image(&reference).await.unwrap().unwrap();
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
        let mut admitted = Vec::new();
        for subject in ["workload", "forwarder"] {
            let (run, limits, privileges) = if subject == "workload" {
                (&state.workload, engine::workload_limits(driver), engine::private_channel_profile(driver).privileges)
            } else {
                (&state.control, engine::control_limits(driver), FORWARDER_PRIVILEGES)
            };
            let execution = ExecutionLease::capture(run.native_pid().unwrap(), limits).unwrap();
            let guard = ProcessLease::capture(run.native_pid().unwrap()).unwrap();
            let changed = ChangedUts::capture(&mut api, run.container_id()).await;
            // These complete fixed values are private, even when explicitly
            // cleared rather than selected by the runtime's default.
            for domain in ["", "(none)", "localdomain"] {
                changed.replace("domainname", domain);
                execution.check().unwrap();
                guarded_tasks(&guard, privileges).unwrap();
            }
            changed.restore();
            for (field, value) in [("hostname", "pbps-804-host.invalid"), ("domainname", "pbps-804-domain.invalid")] {
                changed.replace(field, value);
                run.check().await.unwrap(); // Docker's recipe did not change.
                let execution_accepts = execution.check().is_ok();
                let guard_accepts = guarded_tasks(&guard, privileges).is_ok();
                eprintln!("kernel UTS subject={subject} field={field}, execution={execution_accepts}, guard={guard_accepts}");
                if execution_accepts || guard_accepts { admitted.push((subject, field)); }
                changed.restore();
                execution.check().unwrap();
                guarded_tasks(&guard, privileges).unwrap();
            }
        }
        state.connection.execute("CREATE TABLE pbps_uts_table (id integer)").await.map_err(|e| e.to_string())?;
        state.connection.execute("CREATE VIEW pbps_uts_view AS SELECT id FROM pbps_uts_table").await.map_err(|e| e.to_string())?;
        assert_eq!(state.connection.query("SELECT COUNT(*) FROM pbps_uts_view").await.map_err(|e| e.to_string())?.len(), 1);
        Ok::<_, String>(admitted)
    }.await;
    session
        .close()
        .await
        .expect("both owned containers must be removed");
    assert!(observed.unwrap().is_empty(), "kernel names were admitted");
}
