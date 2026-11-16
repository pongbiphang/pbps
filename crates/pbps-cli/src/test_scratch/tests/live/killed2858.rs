//! A live test killed with its scratch running leaves nothing behind for long:
//! the next live test removes it, and never touches a live one (#2858).

use super::super::stranded2858::{root, sweep_docker};
use super::*;
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, BufReader};

pub(super) const CHILD: &str = "PBPS_KILLED2858_CHILD";
const CASE: &str = "test_scratch::tests::live::a_killed_live_tests_scratch_is_removed_by_the_next_one_and_a_live_ones_is_not";
const SAID: &str = "KILLED2858 ";

/// A live test: its own state directory and a real scratch in it. It says where
/// they are, and then kills itself (`dead`) or waits to be killed (`alive`).
pub(super) async fn child() {
    let d = Dir::live().await;
    let scratch = Box::pin(start_on(&d, Driver::Postgres, &image())).await;
    let cid = scratch.journal.record.container.id.clone().unwrap();
    println!("{SAID}{} {cid}", d.base.display());
    if std::env::var(CHILD).as_deref() == Ok("dead") {
        // Killed as the gate's own timeout or the OOM killer would: no unwinding
        // and no `Drop`, so the scratch and its record are left as they are.
        let _ = std::process::Command::new("kill")
            .args(["-KILL", &std::process::id().to_string()])
            .status();
    }
    tokio::time::sleep(Duration::from_secs(300)).await;
    drop(scratch);
}

struct Started {
    process: tokio::process::Child,
    base: PathBuf,
    cid: String,
}

async fn started(mode: &str) -> Started {
    let mut process = tokio::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            CASE,
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(CHILD, mode)
        .stdout(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut lines = BufReader::new(process.stdout.take().unwrap()).lines();
    let said = tokio::time::timeout(Duration::from_secs(240), async {
        while let Some(line) = lines.next_line().await.unwrap() {
            // `--nocapture` puts the test's output on the line that names it.
            if let Some((_, said)) = line.split_once(SAID) {
                return said.to_owned();
            }
        }
        panic!("the {mode} child ended before it started a scratch");
    })
    .await
    .expect("the child started its scratch in time");
    let (base, cid) = said.split_once(' ').unwrap();
    Started {
        process,
        base: base.into(),
        cid: cid.into(),
    }
}

/// Polls for up to half a minute: another live test process may be removing the
/// same leftover at that moment, and holds its lock while it does.
async fn gone(started: &Started) -> bool {
    for _ in 0..150 {
        if !exists(&started.cid).await && !started.base.exists() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    false
}

pub(super) async fn exercise() {
    let mut alive = started("alive").await;
    let mut dead = started("dead").await;
    let status = dead.process.wait().await.unwrap();
    assert!(!status.success(), "the dead child was killed: {status:?}");
    // The next live test's state directory removes what the killed one left,
    // its container and its directory, and leaves the live one's alone.
    let d = Dir::live().await;
    assert!(
        gone(&dead).await,
        "the killed test's scratch {} and its directory {} are removed",
        dead.cid,
        dead.base.display()
    );
    assert!(
        exists(&alive.cid).await,
        "a live test's scratch is not removed"
    );
    let live_runs = StateDir::open(alive.base.join("test-runs"))
        .unwrap()
        .runs()
        .unwrap();
    assert_eq!(live_runs.len(), 1, "and neither is its record");
    // Killed outright, it is a leftover like the first.
    alive.process.kill().await.unwrap();
    sweep_docker(&root(), &d.base).await;
    assert!(
        gone(&alive).await,
        "the scratch {} killed with its test is removed too",
        alive.cid
    );
}
