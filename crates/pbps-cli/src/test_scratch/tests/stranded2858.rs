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

/// Whether the process that made `base` is provably gone. `Ok(false)` is "not
/// shown to be": no claim at all, or a maker that lives or cannot be told from
/// one. A claim that cannot be read is unknown and reported, never taken for
/// either.
fn maker_gone(base: &Path, boot: &str) -> Result<bool, String> {
    let bytes = match std::fs::read(base.join(MAKER)) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(format!("{}: {e}", base.display())),
    };
    let maker = serde_json::from_slice::<Process>(&bytes)
        .map_err(|e| format!("{}: its claim cannot be read: {e}", base.display()))?;
    Ok(matches!(
        owner::judge(&maker, true, boot, owner::lookup(maker.pid)),
        Owner::Dead(_)
    ))
}

/// The directories directly under `base`, links not followed. A directory that
/// cannot be listed, an entry that cannot be read and an entry whose type cannot
/// be read are errors, never an absence.
fn subdirs(base: &Path) -> Result<Vec<PathBuf>, String> {
    let failed = |path: &Path, e: std::io::Error| format!("{}: {e}", path.display());
    let mut found = Vec::new();
    for entry in std::fs::read_dir(base).map_err(|e| failed(base, e))? {
        let entry = entry.map_err(|e| failed(base, e))?;
        let kind = entry.file_type().map_err(|e| failed(&entry.path(), e))?;
        if kind.is_dir() {
            found.push(entry.path());
        }
    }
    Ok(found)
}

/// Whether a state directory under `base` holds a record, or cannot be read:
/// a live test that leaves one keeps its directory for the next run to find.
pub(super) fn holds_runs(base: &Path) -> bool {
    subdirs(base).map_or(true, |subs| {
        subs.into_iter().any(|sub| {
            StateDir::open(sub)
                .and_then(|dir| dir.runs())
                .map_or(true, |runs| !runs.is_empty())
        })
    })
}

/// Reconciles every state directory under `root` that a dead process made,
/// except `mine`, and removes the ones left empty. A run whose owner holds its
/// lock stays, and so does a directory that still holds a record. Returns what
/// could not be cleaned or read: the caller reports it, and the next run tries
/// again. Only a path that is not there is an empty scan.
pub(super) async fn sweep(
    root: &Path,
    mine: &Path,
    rt: &impl Runtime,
    daemon: &Daemon,
    deadline: Instant,
) -> Vec<String> {
    let mut problems = Vec::new();
    let entries = match std::fs::read_dir(root) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return problems,
        Err(e) => return vec![format!("{}: {e}", root.display())],
    };
    let boot = match owner::boot_id() {
        Ok(boot) => boot,
        Err(e) => return vec![e.to_string()],
    };
    for entry in entries {
        let base = match entry {
            Ok(entry) => entry.path(),
            Err(e) => {
                problems.push(format!("{}: {e}", root.display()));
                continue;
            }
        };
        if base == mine {
            continue;
        }
        match maker_gone(&base, &boot) {
            Ok(true) => {}
            Ok(false) => continue,
            Err(why) => {
                problems.push(why);
                continue;
            }
        }
        let mut kept = false;
        let subs = match subdirs(&base) {
            Ok(subs) => subs,
            Err(why) => {
                problems.push(why);
                continue;
            }
        };
        for sub in subs {
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
    /// A state directory at `base` that this process claims, and that is kept
    /// when it goes out of scope while it still holds a record: a failed
    /// `finish` leaves its run for the next live test to recover.
    pub(super) fn claimed(base: PathBuf) -> Self {
        let mut this = Self::at(base);
        claim(&this.base);
        this.keep = true;
        this
    }

    /// A state directory for a test that starts a real scratch: kept under
    /// [`root`], and made after the leftovers of dead makers there are cleaned.
    pub(super) async fn live() -> Self {
        let root = root();
        let this = Self::claimed(root.join(format!("{:032x}", rand::random::<u128>())));
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

    #[test]
    fn a_live_directory_goes_when_empty_and_stays_while_it_holds_a_record() {
        let outer =
            std::env::temp_dir().join(format!("pbps-stranded2858-{:032x}", rand::random::<u128>()));
        let (kept, gone) = (outer.join("kept"), outer.join("gone"));
        {
            let d = Dir::claimed(kept.clone());
            d.dir
                .write(&record(&run('a'), State::Created, Some(&id('a'))))
                .unwrap();
        }
        assert_eq!(
            StateDir::open(kept.join("test-runs"))
                .unwrap()
                .runs()
                .unwrap(),
            [run('a')],
            "a failed finish's record is left for the next live test"
        );
        assert!(kept.join(MAKER).exists());
        drop(Dir::claimed(gone.clone()));
        assert!(!gone.exists(), "nothing is left to keep");
        let _ = std::fs::remove_dir_all(outer);
    }

    #[tokio::test]
    async fn a_scan_that_cannot_read_is_reported_and_keeps_what_it_could_not_read() {
        use std::os::unix::fs::PermissionsExt;
        let d = Dir::new();
        // Only a path that is not there is an empty scan.
        let rt = Fake::default();
        let missing = d.base.join("no-such-root");
        assert!(
            sweep(&missing, &missing, &rt, &daemon(), soon())
                .await
                .is_empty()
        );
        let file = d.base.join("a-file");
        std::fs::write(&file, b"").unwrap();
        assert_eq!(sweep(&file, &file, &rt, &daemon(), soon()).await.len(), 1);
        // A claim that does not parse is unknown: reported, and nothing is judged.
        let root = d.base.join("live-root");
        let (garbled, dir) = leftover(&root, Some(dead()), &run('b'));
        std::fs::write(garbled.join(MAKER), b"not a claim").unwrap();
        let problems = swept(&root, &runtime_with(&run('b'))).await;
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert_eq!(dir.runs().unwrap(), [run('b')]);
        // A directory that can be entered but not listed cannot be shown empty.
        let (unlisted, _) = leftover(&root, Some(dead()), &run('d'));
        std::fs::set_permissions(&unlisted, std::fs::Permissions::from_mode(0o300)).unwrap();
        if std::fs::read_dir(&unlisted).is_ok() {
            // Root ignores permission bits, so the listing cannot be made to
            // fail; only this case is skipped.
            std::fs::remove_dir_all(&unlisted).unwrap();
        } else {
            assert!(
                holds_runs(&unlisted),
                "an unlistable directory keeps its records"
            );
            let rt = runtime_with(&run('d'));
            let problems = swept(&root, &rt).await;
            assert!(
                problems
                    .iter()
                    .any(|p| p.contains(unlisted.to_str().unwrap())),
                "{problems:?}"
            );
            assert!(rt.removed.borrow().is_empty());
            std::fs::set_permissions(&unlisted, std::fs::Permissions::from_mode(0o700)).unwrap();
            assert!(unlisted.exists());
        }
        // So is a state directory this user cannot show to be private.
        let (open, dir) = leftover(&root, Some(dead()), &run('c'));
        std::fs::set_permissions(
            open.join("test-runs"),
            std::fs::Permissions::from_mode(0o777),
        )
        .unwrap();
        let rt = runtime_with(&run('c'));
        let problems = swept(&root, &rt).await;
        assert!(
            problems.iter().any(|p| p.contains("test-runs")),
            "{problems:?}"
        );
        assert!(rt.removed.borrow().is_empty());
        assert!(open.exists());
        std::fs::set_permissions(
            open.join("test-runs"),
            std::fs::Permissions::from_mode(0o700),
        )
        .unwrap();
        drop(dir);
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
