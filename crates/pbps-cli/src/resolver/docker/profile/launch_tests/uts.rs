//! Empty image files must not hide the kernel-visible UTS names.

use super::*;

#[tokio::test]
#[ignore = "requires the explicit owned rootful Docker fixture"]
async fn kernel_names_are_required_before_engine_initialization() {
    assert!(
        accepts_waiter(|_| {}).await,
        "fixed empty image files qualify"
    );
    assert!(
        accepts_waiter(|launch| launch.body["NetworkDisabled"] = json!(false)).await,
        "fixed generated runtime files qualify"
    );
    let mut admitted = Vec::new();
    for (field, value) in [
        ("Hostname", "pbps-804-host.invalid"),
        ("Domainname", "pbps-804-domain.invalid"),
    ] {
        let accepted = accepts_waiter(|launch| launch.body[field] = json!(value)).await;
        eprintln!("kernel UTS before initialization: {field}, admitted={accepted}");
        if accepted {
            admitted.push(field);
        }
    }
    assert!(
        admitted.is_empty(),
        "kernel names were admitted: {admitted:?}"
    );
}
