//! What a killed live test leaves in its state directory (#2858).
//!
//! A live test that starts a real `TestScratch` keeps its run records in a
//! state directory of its own. When the test process is killed, the record and
//! the container survive and nothing opens that directory again: the
//! resolver's sweep leaves a recorded container to its journal (DEC-2471.1),
//! and `pbps test --cleanup` reconciles only the user's default directory.
//!
//! `Dir::live` keeps those directories in one place per user, so that the next
//! live test finds them, and reconciles the ones whose maker is provably gone
//! with the production reconciliation before it makes its own.

use std::path::Path;

use super::*;

/// The directory of every live test's state directory, next to the default
/// one (`pbps test` and `pbps test --cleanup` use `.../pbps/test-runs`).
pub(super) fn root() -> PathBuf {
    record::default_dir(|name| std::env::var_os(name))
        .expect("XDG_STATE_HOME or HOME must name a directory for the live test state")
        .with_file_name("live-test-runs")
}

const MAKER: &str = "maker.json";

/// Records that this process made `base`. A directory is swept only when its
/// maker is gone, so a sibling another live test process is still using is
/// never judged, even while one of its records looks dead on purpose.
pub(super) fn claim(base: &Path) {
    let temporary = base.join(format!("{MAKER}.new"));
    std::fs::write(
        &temporary,
        serde_json::to_vec(&Process::current().unwrap()).unwrap(),
    )
    .unwrap();
    std::fs::rename(temporary, base.join(MAKER)).unwrap();
}

/// Whether the process that made `base` is provably gone. A directory with no
/// readable claim has not been shown to be anybody's leftover.
fn maker_gone(base: &Path, boot: &str) -> bool {
    let Ok(bytes) = std::fs::read(base.join(MAKER)) else {
        return false;
    };
    let Ok(maker) = serde_json::from_slice::<Process>(&bytes) else {
        return false;
    };
    matches!(
        owner::judge(&maker, true, boot, owner::lookup(maker.pid)),
        Owner::Dead(_)
    )
}

/// Reconciles every state directory under `root` that a dead process made,
/// except `mine`, and removes the ones left empty. A run whose owner holds its
/// lock stays, and so does a directory that still holds a record. Returns what
/// could not be cleaned: the caller reports it, and the next run tries again.
pub(super) async fn sweep(
    root: &Path,
    mine: &Path,
    rt: &impl Runtime,
    daemon: &Daemon,
    deadline: Instant,
) -> Vec<String> {
    let mut problems = Vec::new();
    let Ok(entries) = std::fs::read_dir(root) else {
        return problems;
    };
    let boot = match owner::boot_id() {
        Ok(boot) => boot,
        Err(e) => return vec![e.to_string()],
    };
    for base in entries.flatten().map(|entry| entry.path()) {
        if base == mine || !maker_gone(&base, &boot) {
            continue;
        }
        let mut kept = false;
        let subs = std::fs::read_dir(&base).map(|read| read.flatten().map(|e| e.path()));
        for sub in subs.into_iter().flatten().filter(|path| path.is_dir()) {
            let dir = match StateDir::open(sub.clone()) {
                Ok(dir) => dir,
                Err(e) => {
                    problems.push(format!("{}: {e}", sub.display()));
                    kept = true;
                    continue;
                }
            };
            match reconcile(&dir, rt, daemon, deadline).await {
                Ok(done) => problems.extend(done.failures.iter().map(ToString::to_string)),
                Err(e) => problems.push(format!("{}: {e}", sub.display())),
            }
            kept |= dir.runs().map_or(true, |runs| !runs.is_empty());
        }
        if !kept {
            let _ = std::fs::remove_dir_all(&base);
        }
    }
    problems
}

/// [`sweep`] against the runtime this process talks to, reporting what it
/// could not clean: a leftover is no reason to fail the test that found it.
pub(super) async fn sweep_docker(root: &Path, mine: &Path) {
    let deadline = soon();
    let problems =
        match super::super::discovered(deadline, crate::test_scratch::runtime::current()).await {
            Ok(daemon) => {
                sweep(
                    root,
                    mine,
                    &crate::test_scratch::runtime::Docker,
                    &daemon,
                    deadline,
                )
                .await
            }
            Err(e) => vec![e.to_string()],
        };
    for problem in problems {
        eprintln!("live test state left by a killed run was not cleaned (#2858): {problem}");
    }
}

impl Dir {
    /// A state directory for a test that starts a real scratch: kept under
    /// [`root`], and made after the leftovers of dead makers there are cleaned.
    pub(super) async fn live() -> Self {
        let root = root();
        let this = Self::at(root.join(format!("{:032x}", rand::random::<u128>())));
        claim(&this.base);
        Box::pin(sweep_docker(&root, &this.base)).await;
        this
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dead() -> Process {
        Process {
            pid: GONE,
            start_ticks: 1,
            boot_id: owner::boot_id().unwrap(),
        }
    }

    /// A directory under `root` that `maker` claims, holding the record of a
    /// run whose owner is gone and whose container the runtime still has.
    fn leftover(root: &Path, maker: Option<Process>, run_id: &str) -> (PathBuf, StateDir) {
        let base = root.join(format!("{:032x}", rand::random::<u128>()));
        let dir = StateDir::open(base.join("test-runs")).unwrap();
        if let Some(maker) = maker {
            std::fs::write(base.join(MAKER), serde_json::to_vec(&maker).unwrap()).unwrap();
        }
        dir.write(&record(
            run_id,
            State::Created,
            Some(&id(run_id.chars().next().unwrap())),
        ))
        .unwrap();
        (base, dir)
    }

    fn runtime_with(run_id: &str) -> Fake {
        Fake::holding(&container(run_id, &id(run_id.chars().next().unwrap())))
    }

    async fn swept(root: &Path, rt: &Fake) -> Vec<String> {
        sweep(root, &root.join("mine"), rt, &daemon(), soon()).await
    }

    #[tokio::test]
    async fn the_leftovers_of_a_dead_maker_are_reconciled_and_the_directory_goes() {
        let d = Dir::new();
        let root = d.base.join("live-root");
        let run_id = run('a');
        let (base, _) = leftover(&root, Some(dead()), &run_id);
        let rt = runtime_with(&run_id);
        assert!(swept(&root, &rt).await.is_empty());
        assert_eq!(*rt.removed.borrow(), [id('a')]);
        assert!(!base.exists(), "nothing is left to keep the directory");
    }

    #[tokio::test]
    async fn a_directory_whose_maker_lives_is_not_judged_even_when_a_record_looks_dead() {
        let d = Dir::new();
        let root = d.base.join("live-root");
        let run_id = run('b');
        let (base, dir) = leftover(&root, Some(Process::current().unwrap()), &run_id);
        let rt = runtime_with(&run_id);
        assert!(swept(&root, &rt).await.is_empty());
        assert!(rt.removed.borrow().is_empty() && rt.queried.borrow().is_empty());
        assert_eq!(dir.runs().unwrap(), [run_id]);
        assert!(base.exists());
    }

    #[tokio::test]
    async fn a_directory_nobody_claimed_and_the_sweeping_ones_own_are_not_judged() {
        let d = Dir::new();
        let root = d.base.join("live-root");
        let (unclaimed, _) = leftover(&root, None, &run('c'));
        // The sweeping directory itself, made by a process that is gone, is skipped
        // by name: it is the caller's.
        let own = root.join("mine");
        let own_dir = StateDir::open(own.join("test-runs")).unwrap();
        std::fs::write(own.join(MAKER), serde_json::to_vec(&dead()).unwrap()).unwrap();
        own_dir
            .write(&record(&run('d'), State::Created, Some(&id('d'))))
            .unwrap();
        let rt = Fake {
            containers: vec![
                container(&run('c'), &id('c')),
                container(&run('d'), &id('d')),
            ],
            ..Fake::default()
        };
        assert!(swept(&root, &rt).await.is_empty());
        assert!(rt.removed.borrow().is_empty());
        assert!(unclaimed.exists() && own.exists());
    }

    #[tokio::test]
    async fn a_run_whose_lock_is_held_and_a_failed_removal_keep_their_directory() {
        let d = Dir::new();
        let root = d.base.join("live-root");
        let (held_base, held_dir) = leftover(&root, Some(dead()), &run('e'));
        let _held = held_dir.lock(&run('e')).unwrap();
        let rt = runtime_with(&run('e'));
        assert!(swept(&root, &rt).await.is_empty());
        assert!(rt.removed.borrow().is_empty(), "a run that is held is live");
        assert_eq!(held_dir.runs().unwrap(), [run('e')]);
        assert!(held_base.exists());
        // A removal that cannot be confirmed is reported and the record stays.
        let (failed_base, failed_dir) = leftover(&root, Some(dead()), &run('f'));
        let rt = Fake {
            cannot_remove: Some(vec!["the container".into()]),
            ..runtime_with(&run('f'))
        };
        let problems = swept(&root, &rt).await;
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(problems[0].contains(&run('f')), "{problems:?}");
        assert_eq!(failed_dir.runs().unwrap(), [run('f')]);
        assert!(failed_base.exists());
    }
}
