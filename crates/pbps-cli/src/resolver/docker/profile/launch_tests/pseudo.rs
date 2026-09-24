//! A foreign IPC filesystem must refuse before releasing the bootstrap.

use super::*;
use crate::resolver::docker::pseudo_fixture::ChangedPseudo;
use crate::resolver::native::ExecutionLease;

#[tokio::test]
#[ignore = "requires the explicit owned rootful Docker fixture"]
async fn mqueue_origin_is_required_before_engine_initialization() {
    let refused = inspect_waiter(
        |_| {},
        |run, driver| {
            let pid = run.native_pid().unwrap();
            let lease = ExecutionLease::capture(pid, engine::workload_limits(driver)).unwrap();
            let socket = std::env::var("PBPS_RESOLVER_TEST_SOCKET").unwrap();
            let changed = ChangedPseudo::from_pid(&socket, run.container_id(), pid, "mqueue");
            changed.replace();
            let retained = lease.check().is_err();
            let admission = ExecutionLease::capture(pid, engine::workload_limits(driver)).is_err();
            changed.restore();
            lease.check().unwrap();
            eprintln!(
                "mqueue bootstrap: retained_refused={retained}, admission_refused={admission}"
            );
            retained && admission
        },
    )
    .await;
    assert!(
        refused,
        "a foreign mqueue was admitted before engine initialization"
    );
}
