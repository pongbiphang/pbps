//! Removing a container a test left behind on purpose (#2652).
//!
//! A run that cannot dispose of its scratch container leaves it running with
//! no keeper session. That is exactly a dead run's leftover, and the next run
//! on the same Docker daemon removes it before starting its own (#2410). So a
//! test that cleans such a container up itself races every concurrent run: its
//! `docker rm` can find the container already gone. What the test needs is the
//! container, and the anonymous volumes it held, absent; who removed them is
//! not its concern. A failure names the command's exit code and stderr.
use super::*;
use std::process::Output;

/// How long the container, and then each volume, may take to be confirmed
/// gone after `docker rm` returned or reported another remover at work.
const GONE: Duration = Duration::from_secs(30);

/// Removes the container `id` (a full id, never a name) with its anonymous
/// volumes, and returns only once the runtime reports both gone.
pub fn remove_left_container(docker: &str, id: &str, volumes: &[String]) {
    remove_within(docker, id, volumes, GONE);
}

/// The shim records mounts inside the startup grace, before the run can
/// connect and later release its keeper. After removal, inspect loses the names.
pub fn remove_captured_container(docker: &str, id: &str, created: &std::path::Path) {
    let mut path = created.as_os_str().to_owned();
    path.push(".volumes");
    let captured = std::fs::read_to_string(&path);
    let volumes = match captured {
        Ok(text) => text
            .split_whitespace()
            .map(str::to_owned)
            .collect::<Vec<_>>(),
        Err(error) => {
            // Still attempt our owned cleanup, but never accept unknown mounts
            // as an empty set and accidentally qualify a surviving volume.
            let removal = run(docker, &["rm", "--force", "--volumes", id]);
            panic!(
                "volume capture for container {id} could not be read: {error}; rm {}",
                said(&removal)
            );
        }
    };
    remove_left_container(docker, id, &volumes);
}

fn run(docker: &str, args: &[&str]) -> Output {
    // A script written a moment ago can still be busy to exec (ETXTBSY).
    stand_in::retry_exec(|| Command::new(docker).args(args).output()).unwrap()
}

fn said(out: &Output) -> String {
    format!(
        "exit {:?}, stderr {:?}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr).trim()
    )
}

fn remove_within(docker: &str, id: &str, volumes: &[String], within: Duration) {
    let run = |args: &[&str]| run(docker, args);
    let removal = run(&["rm", "--force", "--volumes", id]);
    if !removal.status.success() {
        // Only another remover at work is tolerated; what is checked next is
        // that the container really is gone.
        let why = String::from_utf8_lossy(&removal.stderr).to_ascii_lowercase();
        assert!(
            why.contains("no such container") || why.contains("already in progress"),
            "docker rm --force --volumes {id} failed, {}",
            said(&removal)
        );
    }
    let removal_summary = format!("rm {}", said(&removal));
    until_absent(
        &format!("container {id}"),
        "no such container",
        within,
        &removal_summary,
        || run(&["container", "inspect", "--format", "{{.State.Status}}", id]),
    );
    for volume in volumes {
        until_absent(
            &format!("volume {volume} of container {id}"),
            "no such volume",
            within,
            &removal_summary,
            || run(&["volume", "inspect", "--format", "{{.Name}}", volume]),
        );
    }
}

/// Polls `inspect` until the runtime says `missing`. Any other failure is a
/// failure, not absence, and so is a resource still there after `within`.
fn until_absent(
    what: &str,
    missing: &str,
    within: Duration,
    removal: &str,
    inspect: impl Fn() -> Output,
) {
    let started = Instant::now();
    loop {
        let out = inspect();
        if !out.status.success() {
            let why = String::from_utf8_lossy(&out.stderr).to_ascii_lowercase();
            assert!(
                why.contains(missing),
                "whether {what} is gone could not be read: exit {:?}, stderr {:?}; {removal}",
                out.status.code(),
                why.trim()
            );
            return;
        }
        assert!(
            started.elapsed() < within,
            "{what} is still there {}ms after its removal: {}; {removal}",
            within.as_millis(),
            String::from_utf8_lossy(&out.stdout).trim()
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[cfg(unix)]
mod tests {
    use super::*;

    const ID: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    const SHORT: Duration = Duration::from_millis(300);
    const NO_CONTAINER: &str =
        "echo 'Error response from daemon: No such container: x' >&2; exit 1";
    const NO_VOLUME: &str = "echo 'Error response from daemon: get v: no such volume' >&2; exit 1";

    /// A `docker` that answers by the `case` arms given, keyed on its first
    /// two arguments; `$n` in an arm is the number of that arm's calls so far.
    fn fake(name: &str, arms: &str) -> (Demo, String) {
        let dir = Demo::new(name);
        let path = dir.dir.join("docker");
        executable(
            &path,
            &format!(
                "#!/bin/sh\nd=$(dirname \"$0\")\n\
                 n=$(cat \"$d/n.$1.$2\" 2>/dev/null || echo 0); n=$((n + 1)); echo $n > \"$d/n.$1.$2\"\n\
                 case \"$1 $2\" in\n{arms}\n*) exit 99;;\nesac\n"
            ),
        );
        let path = path.display().to_string();
        (dir, path)
    }

    fn caught_failure(action: impl FnOnce() + std::panic::UnwindSafe) -> String {
        let caught = std::panic::catch_unwind(action).unwrap_err();
        caught
            .downcast_ref::<String>()
            .cloned()
            .or_else(|| caught.downcast_ref::<&str>().map(|s| (*s).to_owned()))
            .unwrap()
    }

    fn failure(docker: &str) -> String {
        caught_failure(|| remove_within(docker, ID, &["vol1".into()], SHORT))
    }

    /// The run's container was removed by a later run's recovery sweep before
    /// the test's own `docker rm`: the race of #2652, made without a daemon.
    /// The former assertion (`rm` exits 0) fails here.
    #[test]
    fn a_container_taken_first_by_another_remover_is_gone_not_a_failure() {
        let (_dir, docker) = fake(
            "trace2652-taken",
            &format!(
                "\"container inspect\") {NO_CONTAINER};;\n\"rm --force\") {NO_CONTAINER};;\n\"volume inspect\") {NO_VOLUME};;"
            ),
        );
        remove_within(&docker, ID, &["vol1".into()], SHORT);
    }

    /// Another remover is mid-removal: `rm` says so, and the container and
    /// its volume are waited for until the runtime reports both gone.
    #[test]
    fn a_removal_in_progress_is_waited_for_with_its_volume() {
        let (_dir, docker) = fake(
            "trace2652-progress",
            &format!(
                "\"container inspect\") case $n in 1) echo removing;; *) {NO_CONTAINER};; esac;;\n\
                 \"rm --force\") echo 'removal of container x is already in progress' >&2; exit 1;;\n\
                 \"volume inspect\") case $n in 1) echo vol1;; *) {NO_VOLUME};; esac;;"
            ),
        );
        remove_within(&docker, ID, &["vol1".into()], Duration::from_secs(5));
    }

    /// A failed `rm` that is not another remover is the failure, with the exit
    /// code and stderr the former assertion threw away.
    #[test]
    fn a_failed_rm_that_is_not_another_remover_names_its_exit_code_and_stderr() {
        let (_dir, docker) = fake(
            "trace2652-denied",
            "\"container inspect\") echo running;;\n\"rm --force\") echo 'permission denied' >&2; exit 125;;",
        );
        let message = failure(&docker);
        assert!(
            message.contains("exit Some(125)") && message.contains("permission denied"),
            "{message}"
        );
    }

    /// "No such container" from `rm` proves nothing while the container still
    /// answers an inspect.
    #[test]
    fn a_container_that_still_answers_after_rm_is_not_gone() {
        let (_dir, docker) = fake(
            "trace2652-present",
            &format!("\"container inspect\") echo running;;\n\"rm --force\") {NO_CONTAINER};;"),
        );
        let message = failure(&docker);
        assert!(
            message.contains("is still there") && message.contains("rm exit Some(1)"),
            "{message}"
        );
    }

    /// The container is gone but its anonymous volume is not.
    #[test]
    fn a_volume_that_outlives_its_container_is_not_gone() {
        let (_dir, docker) = fake(
            "trace2652-volume",
            &format!(
                "\"container inspect\") {NO_CONTAINER};;\n\
                 \"rm --force\") exit 0;;\n\
                 \"volume inspect\") echo vol1;;"
            ),
        );
        let message = failure(&docker);
        assert!(
            message.contains("volume vol1") && message.contains("still there"),
            "{message}"
        );
    }

    /// A daemon that cannot be read says nothing of absence, whether it fails
    /// before the removal or while the container is being confirmed gone.
    #[test]
    fn an_unreadable_daemon_is_not_absence() {
        let (_dir, docker) = fake(
            "trace2652-unreadable",
            "\"rm --force\") echo 'Cannot connect to the Docker daemon' >&2; exit 1;;",
        );
        assert!(failure(&docker).contains("Cannot connect"));
        let (_dir, docker) = fake(
            "trace2652-unreadable-later",
            "\"container inspect\") echo 'Cannot connect to the Docker daemon' >&2; exit 1;;\n\"rm --force\") exit 0;;",
        );
        let message = failure(&docker);
        assert!(
            message.contains("could not be read") && message.contains("cannot connect"),
            "{message}"
        );
    }
    #[test]
    fn an_already_removed_container_does_not_hide_a_surviving_volume() {
        let (_dir, docker) = fake(
            "trace2652-taken-volume",
            &format!(
                "\"container inspect\") {NO_CONTAINER};;\n\"rm --force\") {NO_CONTAINER};;\n\"volume inspect\") echo vol1;;"
            ),
        );
        let message = failure(&docker);
        assert!(
            message.contains("volume vol1")
                && message.contains("still there")
                && message.contains("rm exit Some(1)"),
            "{message}"
        );
    }

    #[test]
    fn an_unreadable_volume_capture_is_not_an_empty_mount_set() {
        let (dir, docker) = fake("trace2652-no-capture", "\"rm --force\") exit 0;;");
        let created = dir.dir.join("created");
        for invalid in [None, Some(vec![0xff])] {
            if let Some(bytes) = invalid {
                std::fs::write(dir.dir.join("created.volumes"), bytes).unwrap();
            }
            let message = caught_failure(|| remove_captured_container(&docker, ID, &created));
            assert!(
                message.contains("volume capture")
                    && message.contains("could not be read")
                    && message.contains("rm exit Some(0)"),
                "{message}"
            );
        }
    }

    #[test]
    fn a_readable_empty_volume_capture_qualifies_no_mounts() {
        let (dir, docker) = fake(
            "trace2652-empty-capture",
            &format!("\"container inspect\") {NO_CONTAINER};;\n\"rm --force\") {NO_CONTAINER};;"),
        );
        let created = dir.dir.join("created");
        std::fs::write(dir.dir.join("created.volumes"), "").unwrap();
        remove_captured_container(&docker, ID, &created);
    }
}
