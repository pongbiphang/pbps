//! A killed run's container on a host shared with other gate shards can be
//! removed by another remover between this sweep's listing and its identity
//! read (#2807). DEC-2513.1 keeps that as a named warning, because a container
//! that vanished cannot give the volumes it owned. So a live test that kills a
//! container and sweeps may see either outcome, and what it must still demand
//! is that the container and every volume it owned are confirmed absent.

use super::container::{no_such_container, no_such_volume};
use std::process::Command;
use std::time::{Duration, Instant};

/// The warning a sweep keeps for a container that vanished before its
/// identity read (`recovery.rs`, `classify`), for `name` and nothing else.
pub(super) fn lost_to_another_remover(why: &str, name: &str) -> bool {
    why.starts_with("its container identity and volumes could not be read before removal: ")
        && no_such_container(why)
        && why.contains(name)
}

/// Whether the reasons a sweep kept a killed run's container are ones it may
/// have: none (this sweep removed it, or another remover finished before the
/// listing) or only the warning above (another remover finished after it). Any
/// other reason is a failure, whoever else was removing; that the container and
/// its volumes are gone is [`confirmed_absent`]'s question, not this one's.
pub(super) fn accept_kept(kept: &[String], name: &str) -> Result<(), String> {
    match kept.iter().find(|why| !lost_to_another_remover(why, name)) {
        Some(other) => Err(format!("{name} was kept for another reason: {other}")),
        None => Ok(()),
    }
}

/// `Ok(true)` when the runtime says `args` names nothing, `Ok(false)` when it
/// exists, and `Err` for any other failure: an unreadable answer is not an
/// absent one.
fn missing(args: &[&str], is_missing: fn(&str) -> bool) -> Result<bool, String> {
    let output = Command::new("docker")
        .args(args)
        .output()
        .map_err(|error| format!("cannot run docker: {error}"))?;
    if output.status.success() {
        return Ok(false);
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    if is_missing(&stderr) {
        Ok(true)
    } else {
        Err(format!("docker {args:?} failed unreadably: {stderr}"))
    }
}

/// Waits up to `within` for the container and every volume it owned to be
/// confirmed absent by the runtime's own answer; a remover that is still
/// deleting the writable layer is waited for, a survivor is named.
pub(super) async fn confirmed_absent_within(
    name: &str,
    volumes: &[String],
    within: Duration,
) -> Result<(), String> {
    let deadline = Instant::now() + within;
    loop {
        let mut left = Vec::new();
        if !missing(&["container", "inspect", name], no_such_container)? {
            left.push(format!("the container {name}"));
        }
        for volume in volumes {
            if !missing(&["volume", "inspect", volume], no_such_volume)? {
                left.push(format!("the volume {volume}"));
            }
        }
        if left.is_empty() {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(format!("still there: {}", left.join(", ")));
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

pub(super) async fn confirmed_absent(name: &str, volumes: &[String]) -> Result<(), String> {
    confirmed_absent_within(name, volumes, Duration::from_secs(20)).await
}

#[cfg(test)]
mod tests {
    use super::*;

    const NAME: &str = "pbps-scratch-0123456789abcdef0123456789abcdef";

    fn unread(error: &str) -> String {
        format!(
            "its container identity and volumes could not be read before removal: docker container failed (exit status: 1): {error}"
        )
    }

    #[test]
    fn another_removers_win_is_the_warning_for_this_container_in_either_runtime_spelling() {
        let docker = unread(&format!(
            "Error response from daemon: No such container: {NAME}"
        ));
        let podman = unread(&format!("Error: no such container \"{NAME}\""));
        for why in [docker, podman] {
            assert!(lost_to_another_remover(&why, NAME), "{why}");
            assert_eq!(accept_kept(&[why], NAME), Ok(()));
        }
    }

    #[test]
    fn a_removal_by_this_sweep_or_before_its_listing_keeps_nothing_and_is_accepted() {
        assert_eq!(accept_kept(&[], NAME), Ok(()));
    }

    #[test]
    fn every_other_reason_to_keep_the_container_still_fails() {
        for why in [
            // The read failed, but the daemon did not say the container is gone.
            unread("Error response from daemon: permission denied"),
            unread(&format!(
                "Error response from daemon: permission denied on {NAME}"
            )),
            unread("Cannot connect to the Docker daemon at unix:///var/run/docker.sock"),
            // Another container's absence is no evidence about this one.
            unread("Error response from daemon: No such container: pbps-scratch-ffff"),
            // A different warning that happens to mention the same name.
            format!("its removal has not yet been confirmed: No such container: {NAME}"),
            "its container identity changed".to_owned(),
            format!("it is exited, No such container: {NAME}"),
        ] {
            assert!(!lost_to_another_remover(&why, NAME), "{why}");
            assert!(
                accept_kept(std::slice::from_ref(&why), NAME).is_err(),
                "{why}"
            );
        }
    }

    #[test]
    fn one_other_reason_among_the_warnings_fails() {
        let lost = unread(&format!(
            "Error response from daemon: No such container: {NAME}"
        ));
        let other = "its server could not be read: reset".to_owned();
        assert!(accept_kept(&[lost, other], NAME).is_err());
    }
}
