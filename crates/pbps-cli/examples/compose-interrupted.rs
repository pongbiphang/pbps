//! A compose that stops where it is told, so recovery can be measured rather
//! than reasoned about (DECISIONS 525).
//!
//! ADR-0015's Limits section requires step 4 to interrupt a compose at each
//! durable point and assert what recovery does with what it finds. An
//! interruption has to be a real one: a process that publishes a phase and
//! then *stops* — not a test that writes the record it imagines that process
//! would have left, which would measure the test's idea of the state rather
//! than the state.
//!
//! So this runs the ordinary compose through its ordinary public API and calls
//! `abort` from the phase watcher. Nothing here is a second implementation of
//! the protocol, and nothing here is compiled into `pbps`: an example is built
//! by `cargo test --all-targets` and by nothing that ships.
//!
//! Usage: `compose-interrupted <pbps> <checkout> <phase> <from> <to>`, where
//! `phase` is one of `locking`, `placing`, `composed`, `installed`,
//! `rolling-back`, or anything else for "do not stop". The binary's path is an
//! argument because `CARGO_BIN_EXE_*` is set for tests and not for examples,
//! and the test that runs this knows where it is.

use std::path::PathBuf;
use std::time::Duration;

use pbps_ui::compose::cli::{Cli, Intent};
use pbps_ui::compose::git::Git;
use pbps_ui::compose::record::Phase;
use pbps_ui::compose::repo_path::RepoPath;
use pbps_ui::compose::run::{Compose, Request};

fn main() {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    let [executable, checkout, phase, from, to] = arguments.as_slice() else {
        eprintln!("usage: compose-interrupted <pbps> <checkout> <phase> <from> <to>");
        std::process::exit(64);
    };
    let checkout = PathBuf::from(checkout);
    let private = checkout.join(".git").join("pbps-ui-private");
    std::fs::create_dir_all(&private).expect("a private directory");
    let git = Git::new(checkout.clone(), &private, Duration::from_secs(60)).expect("git");
    let cli = Cli {
        executable: PathBuf::from(executable),
        deadline: Duration::from_secs(60),
    };
    let project_file = RepoPath::new(b"pbps.yml").expect("a path");
    let stop_at = match phase.as_str() {
        "locking" => Some(Phase::Locking),
        "placing" => Some(Phase::Placing),
        "composed" => Some(Phase::Composed),
        "installed" => Some(Phase::Installed),
        "rolling-back" => Some(Phase::RollingBack),
        _ => None,
    };
    let watching = move |reached: Phase| {
        if Some(reached) == stop_at {
            // `abort`, not `exit`: the point is a process that stops without
            // unwinding, leaving exactly what was on disk when it stopped.
            std::process::abort();
        }
    };
    let compose = Compose {
        git: &git,
        cli: &cli,
        project: None,
        project_file: &project_file,
        git_dir: checkout.join(".git"),
        remote_name: "pbps-ui-interrupted".to_owned(),
        watching: Some(&watching),
    };
    match compose.run(&Request {
        intent: Intent::Rename {
            from: from.clone(),
            to: to.clone(),
        },
        message: format!("rename {from} {to}"),
        remote: String::new(),
        shown: Default::default(),
    }) {
        Ok(composed) => println!("composed {}", composed.commit),
        Err(refusal) => {
            eprintln!("refused: {refusal}");
            std::process::exit(1);
        }
    }
}
