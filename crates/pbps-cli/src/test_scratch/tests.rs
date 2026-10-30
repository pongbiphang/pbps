use std::cell::RefCell;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use super::owner::{self, Owner, Process};
use super::reconcile::reconcile;
use super::record::{self, Container, Daemon, Record, State, StateDir, VERSION};
use super::removal::RUN_LABEL;
use super::runtime::{Fail, Inspected, Runtime};
use super::{FailureKind, TestScratch, settle_failed_start, state_dir};

/// A state directory in its own temporary directory, removed with it.
///
/// Tests that hold a run lock run one at a time: a lock belongs to the open
/// file, not the process, so a `docker` child forked by a concurrent test while
/// the lock is held keeps it held until the child execs, and the next
/// reconciliation would read a dead run as live.
struct Dir {
    base: PathBuf,
    dir: StateDir,
    _serial: std::sync::MutexGuard<'static, ()>,
}

impl Dir {
    fn new() -> Self {
        static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let serial = SERIAL
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let base =
            std::env::temp_dir().join(format!("pbps-test-scratch-{:032x}", rand::random::<u128>()));
        let dir = StateDir::open(base.join("test-runs")).unwrap();
        Self {
            base,
            dir,
            _serial: serial,
        }
    }
}

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.base);
    }
}

/// A pid above any `pid_max`: no such process.
const GONE: u32 = 2_000_000_000;

fn daemon() -> Daemon {
    Daemon {
        endpoint: "default".into(),
        identity: "test".into(),
    }
}

fn record(run_id: &str, state: State, id: Option<&str>) -> Record {
    Record {
        version: VERSION,
        run_id: run_id.into(),
        purpose: "plan-test".into(),
        engine: "postgres".into(),
        process: Process {
            pid: GONE,
            start_ticks: 1,
            boot_id: owner::boot_id().unwrap(),
        },
        daemon: daemon(),
        container: Container {
            name: format!("pbps-scratch-{run_id}"),
            id: id.map(str::to_owned),
        },
        volumes: vec!["vol1".into()],
        leftovers: Vec::new(),
        labels: BTreeMap::from([
            ("pbps.resolver".to_owned(), "scratch".to_owned()),
            (RUN_LABEL.to_owned(), run_id.to_owned()),
        ]),
        state,
    }
}

fn id(n: char) -> String {
    n.to_string().repeat(64)
}

fn run(n: char) -> String {
    n.to_string().repeat(32)
}

/// Whether the run's lock can be taken, allowing for a `docker` or `git` child
/// that another test's thread forked while the lock was held: a lock belongs to
/// the open file, so it is held until that child execs (a few milliseconds).
fn released(dir: &StateDir, run_id: &str) -> bool {
    (0..500).any(|_| {
        let free = dir.try_lock_existing(run_id).unwrap().is_some();
        if !free {
            std::thread::sleep(Duration::from_millis(10));
        }
        free
    })
}

fn soon() -> Instant {
    Instant::now() + Duration::from_secs(60)
}

/// What the runtime holds, and how it fails.
#[derive(Default)]
struct Fake {
    containers: Vec<Inspected>,
    list_fails: Option<Fail>,
    inspect_fails: Option<Fail>,
    volume_users: BTreeMap<String, Vec<String>>,
    cannot_remove: Option<Vec<String>>,
    /// Volumes the runtime still holds after their container went.
    orphans: Vec<String>,
    cannot_remove_volume: bool,
    removed: RefCell<Vec<String>>,
}

impl Fake {
    fn holding(container: &Inspected) -> Self {
        Self {
            containers: vec![container.clone()],
            ..Self::default()
        }
    }
}

fn container(run_id: &str, cid: &str) -> Inspected {
    Inspected {
        id: cid.into(),
        name: format!("pbps-scratch-{run_id}"),
        labels: BTreeMap::from([
            ("pbps.resolver".to_owned(), "scratch".to_owned()),
            (RUN_LABEL.to_owned(), run_id.to_owned()),
        ]),
        volumes: vec!["vol1".into()],
        other_mounts: Vec::new(),
    }
}

impl Runtime for Fake {
    async fn list(&self, label: &str) -> Result<Vec<String>, Fail> {
        if let Some(fail) = &self.list_fails {
            return Err(fail.clone());
        }
        let (key, value) = label.split_once('=').unwrap();
        Ok(self
            .containers
            .iter()
            .filter(|c| c.labels.get(key).map(String::as_str) == Some(value))
            .map(|c| c.id.clone())
            .collect())
    }

    async fn inspect(&self, id: &str) -> Result<Option<Inspected>, Fail> {
        if let Some(fail) = &self.inspect_fails {
            return Err(fail.clone());
        }
        Ok(self.containers.iter().find(|c| c.id == id).cloned())
    }

    async fn users(&self, volume: &str) -> Result<Vec<String>, Fail> {
        Ok(self.volume_users.get(volume).cloned().unwrap_or_default())
    }

    async fn remove_volume(&self, volume: &str) -> Result<(), Vec<String>> {
        if self.cannot_remove_volume {
            return Err(vec![format!("the docker volume {volume}")]);
        }
        if self.orphans.iter().any(|v| v == volume) {
            self.removed.borrow_mut().push(volume.to_owned());
        }
        Ok(())
    }

    async fn remove(&self, id: &str) -> Result<(), Vec<String>> {
        if let Some(left) = &self.cannot_remove {
            return Err(left.clone());
        }
        self.removed.borrow_mut().push(id.to_owned());
        Ok(())
    }
}

async fn reconciled(dir: &Dir, rt: &Fake) -> super::Reconciliation {
    reconcile(&dir.dir, rt, &daemon(), soon()).await.unwrap()
}

fn kept(dir: &Dir, run_id: &str) -> bool {
    dir.dir.runs().unwrap().contains(&run_id.to_owned())
}

mod identity {
    use super::*;

    #[test]
    fn run_ids_are_128_random_bits_in_lower_case_hex() {
        let ids: std::collections::BTreeSet<_> = (0..1000).map(|_| record::new_run_id()).collect();
        assert_eq!(ids.len(), 1000, "a run id repeated");
        assert!(ids.iter().all(|id| record::is_run_id(id)));
        // Nothing of the pid or the clock is in it.
        assert!(
            !ids.iter()
                .any(|id| id.contains(&std::process::id().to_string()) && id.starts_with("0000"))
        );
        // Negative: only exactly 32 lower-case hex digits name a run.
        for bad in [
            "",
            "abc",
            &"A".repeat(32),
            &"g".repeat(32),
            &"a".repeat(31),
            &"a".repeat(33),
            "../etc/passwd",
        ] {
            assert!(!record::is_run_id(bad), "{bad}");
        }
    }

    #[test]
    fn a_process_is_the_same_one_only_with_its_pid_start_and_boot() {
        let here = Process::current().unwrap();
        let boot = here.boot_id.clone();
        // A held lock is the whole answer for a live run.
        assert_eq!(owner::judge(&here, false, &boot, Ok(None)), Owner::Alive);
        assert!(matches!(
            owner::judge(&here, true, "another boot", Ok(Some(here.start_ticks))),
            Owner::Dead(_)
        ));
        assert!(matches!(
            owner::judge(&here, true, &boot, Ok(None)),
            Owner::Dead(_)
        ));
        assert!(matches!(
            owner::judge(&here, true, &boot, Ok(Some(here.start_ticks + 1))),
            Owner::Dead(_)
        ));
        // Negative: the same process with a free lock, or one that cannot be
        // read, proves nothing.
        assert!(matches!(
            owner::judge(&here, true, &boot, Ok(Some(here.start_ticks))),
            Owner::Unprovable(_)
        ));
        let unreadable = Err(super::super::Error::State("denied".into()));
        assert!(matches!(
            owner::judge(&here, true, &boot, unreadable),
            Owner::Unprovable(_)
        ));
        assert_eq!(owner::lookup(GONE).unwrap(), None);
        assert_eq!(
            owner::lookup(std::process::id()).unwrap(),
            Some(here.start_ticks)
        );
    }

    #[test]
    fn the_start_tick_is_read_past_a_command_name_with_spaces_and_parentheses() {
        let stat = |name: &str| {
            format!(
                "42 ({name}) S 1 42 42 0 -1 4194560 100 0 0 0 1 2 0 0 20 0 1 0 987654 1000 10 18446744073709551615"
            )
        };
        for name in ["pbps", "a b", "x) S 1 (y", "(("] {
            assert_eq!(owner::start_ticks(&stat(name)), Some(987654), "{name}");
        }
        for bad in [
            "",
            "42 (pbps)",
            "42 (pbps) S 1 2",
            "no parentheses at all",
            "42 (pbps) S 1 42 42 0 -1 4194560 100 0 0 0 1 2 0 0 20 0 1 0 notanumber 1",
        ] {
            assert_eq!(owner::start_ticks(bad), None, "{bad}");
        }
    }
}

mod records {
    use super::*;

    #[test]
    fn a_record_round_trips_and_refuses_what_it_does_not_know() {
        let d = Dir::new();
        let rec = record(&run('a'), State::Created, Some(&id('b')));
        d.dir.write(&rec).unwrap();
        assert_eq!(d.dir.read(&run('a')).unwrap(), Some(rec.clone()));
        assert_eq!(
            d.dir.read(&run('c')).unwrap(),
            None,
            "absent is not an error"
        );
        let path = d.dir.path().join(format!("{}.json", run('a')));
        let text = std::fs::read_to_string(&path).unwrap();
        // Negative: an unknown field, another version, another run's id,
        // trailing text and a half-written file are all refused, never read as
        // absent.
        let mut other = serde_json::from_str::<serde_json::Value>(&text).unwrap();
        other["extra"] = true.into();
        let bad_version = text.replace("\"version\": 1", "\"version\": 2");
        let bad_run = text.replace(&run('a'), &run('d'));
        for bad in [
            other.to_string(),
            bad_version,
            bad_run,
            format!("{text}trailing"),
            text[..text.len() / 2].to_owned(),
        ] {
            std::fs::write(&path, bad).unwrap();
            assert!(d.dir.read(&run('a')).is_err());
        }
    }

    #[test]
    fn a_record_that_is_a_link_or_not_a_file_is_refused() {
        use std::os::unix::fs::symlink;
        let d = Dir::new();
        // A link to a perfectly good record is still refused.
        d.dir
            .write(&record(&run('a'), State::Pending, None))
            .unwrap();
        let path = d.dir.path().join(format!("{}.json", run('a')));
        let elsewhere = d.base.join("elsewhere.json");
        std::fs::rename(&path, &elsewhere).unwrap();
        symlink(&elsewhere, &path).unwrap();
        assert!(d.dir.read(&run('a')).is_err(), "a link is not followed");
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap();
        assert!(
            d.dir.read(&run('a')).is_err(),
            "a directory is not a record"
        );
    }

    #[test]
    fn invocations_opening_a_new_state_directory_together_all_succeed() {
        let base =
            std::env::temp_dir().join(format!("pbps-test-scratch-{:032x}", rand::random::<u128>()));
        let path = base.join("test-runs");
        let barrier = std::sync::Barrier::new(16);
        let opened: Vec<bool> = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..16)
                .map(|_| {
                    scope.spawn(|| {
                        barrier.wait();
                        StateDir::open(path.clone()).is_ok()
                    })
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });
        let _ = std::fs::remove_dir_all(&base);
        assert!(opened.iter().all(|ok| *ok), "{opened:?}");
    }

    #[test]
    fn the_state_directory_is_private_and_never_a_link() {
        use std::os::unix::fs::{PermissionsExt as _, symlink};
        let d = Dir::new();
        let mode = std::fs::metadata(d.dir.path())
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o700);
        // Negative: open to the group, a link, and a file are refused.
        std::fs::set_permissions(d.dir.path(), std::fs::Permissions::from_mode(0o770)).unwrap();
        assert!(StateDir::open(d.dir.path().to_owned()).is_err());
        std::fs::set_permissions(d.dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let link = d.base.join("link");
        symlink(d.dir.path(), &link).unwrap();
        assert!(StateDir::open(link).is_err());
        let file = d.base.join("file");
        std::fs::write(&file, "").unwrap();
        assert!(StateDir::open(file).is_err());
    }

    #[test]
    fn a_replacement_leaves_the_old_record_or_the_new_and_clears_a_stray_temporary() {
        let d = Dir::new();
        d.dir
            .write(&record(&run('a'), State::Pending, None))
            .unwrap();
        // A kill between writing the temporary and renaming it.
        let stray = d.dir.path().join(format!("{}.json.tmp", run('a')));
        std::fs::write(&stray, "{ torn").unwrap();
        assert_eq!(
            d.dir.read(&run('a')).unwrap().unwrap().state,
            State::Pending
        );
        d.dir
            .write(&record(&run('a'), State::Created, Some(&id('b'))))
            .unwrap();
        assert_eq!(
            d.dir.read(&run('a')).unwrap().unwrap().state,
            State::Created
        );
        assert!(!stray.exists());
        assert_eq!(d.dir.runs().unwrap(), vec![run('a')]);
        // Names that are not a run id's are left alone.
        std::fs::write(d.dir.path().join("notes.json"), "").unwrap();
        assert_eq!(d.dir.runs().unwrap(), vec![run('a')]);
    }

    #[test]
    fn a_held_lock_is_a_live_run_and_a_run_id_is_never_taken_twice() {
        let d = Dir::new();
        let held = d.dir.lock(&run('a')).unwrap();
        assert!(d.dir.lock(&run('a')).is_err(), "a run id is never reused");
        assert!(d.dir.try_lock_existing(&run('a')).unwrap().is_none());
        drop(held);
        assert!(released(&d.dir, &run('a')));
    }

    /// A recovery that finds the lock by name never finds one it can take while
    /// the run that made it is alive, even as that run is only starting.
    #[test]
    fn a_lock_is_never_visible_before_it_is_held() {
        let d = Dir::new();
        for round in 0..3000 {
            let id = format!("{round:032x}");
            let path = d.dir.path().join(format!("{id}.lock"));
            let (done, watcher_id) = (std::sync::atomic::AtomicBool::new(false), id.clone());
            std::thread::scope(|scope| {
                let spy = scope.spawn(|| {
                    // Recovery's own move: take any lock it can see, and delete it.
                    let mut stolen = 0;
                    while !done.load(std::sync::atomic::Ordering::Acquire) {
                        if path.exists()
                            && let Some(file) = d.dir.try_lock_existing(&watcher_id).unwrap()
                        {
                            stolen += 1;
                            drop(file);
                        }
                    }
                    stolen
                });
                let held = d.dir.lock(&id);
                // Stop the spy before anything can panic, or the scope never ends.
                done.store(true, std::sync::atomic::Ordering::Release);
                assert_eq!(spy.join().unwrap(), 0, "a recovery took a live run's lock");
                drop(held.expect("the lock is made"));
            });
            assert!(released(&d.dir, &id));
        }
    }

    #[test]
    fn the_state_directory_follows_the_xdg_rule() {
        let env = |pairs: &'static [(&str, &str)]| {
            move |name: &str| {
                pairs
                    .iter()
                    .find(|(k, _)| *k == name)
                    .map(|(_, v)| (*v).into())
            }
        };
        let path = |pairs| record::default_dir(env(pairs)).unwrap();
        assert_eq!(
            path(&[("XDG_STATE_HOME", "/s"), ("HOME", "/h")]),
            PathBuf::from("/s/pbps/test-runs")
        );
        assert_eq!(
            path(&[("HOME", "/h")]),
            PathBuf::from("/h/.local/state/pbps/test-runs")
        );
        // Negative: a relative XDG_STATE_HOME is ignored, and no home is an error.
        assert_eq!(
            path(&[("XDG_STATE_HOME", "rel"), ("HOME", "/h")]),
            PathBuf::from("/h/.local/state/pbps/test-runs")
        );
        assert!(record::default_dir(env(&[])).is_err());
        // Opened from the environment it is created private under that base.
        let base = std::env::temp_dir().join(format!("pbps-xdg-{:032x}", rand::random::<u128>()));
        let var = |name: &str| (name == "XDG_STATE_HOME").then(|| base.clone().into_os_string());
        let opened = state_dir(var).unwrap();
        assert_eq!(opened.path(), base.join("pbps/test-runs"));
        std::fs::remove_dir_all(&base).unwrap();
        assert!(record::default_dir(env(&[("HOME", "relative")])).is_err());
    }
}

mod recovery {
    use super::super::{Journal, RunOwner};
    use super::*;
    use pbps_cli::resolver::server::scratch::container::Owner as _;

    #[tokio::test]
    async fn the_record_exists_before_the_container_is_asked_for_and_carries_the_run_label() {
        let d = Dir::new();
        let rec = record(&run('a'), State::Pending, None);
        let mut journal = Journal {
            dir: &d.dir,
            record: rec.clone(),
            _lock: d.dir.lock(&run('a')).unwrap(),
        };
        assert_eq!(
            d.dir.read(&run('a')).unwrap(),
            None,
            "nothing is written before the start"
        );
        let mut owner = RunOwner {
            journal: &mut journal,
        };
        owner.before_create().await.unwrap();
        assert_eq!(d.dir.read(&run('a')).unwrap(), Some(rec));
        assert_eq!(owner.token(), run('a'));
        assert_eq!(
            owner.labels(),
            vec![format!("{RUN_LABEL}={}", run('a'))],
            "the resolver sets its own"
        );
        // Negative: an id that is not a container id is refused and recorded nowhere.
        assert!(owner.created("not-an-id").await.is_err());
        assert_eq!(
            d.dir.read(&run('a')).unwrap().unwrap().state,
            State::Pending
        );
    }

    #[tokio::test]
    async fn a_start_that_reported_leftovers_keeps_its_record_whatever_the_listing_says() {
        use pbps_cli::resolver::server::ProduceError;
        let d = Dir::new();
        let rec = record(&run('a'), State::Pending, None);
        d.dir.write(&rec).unwrap();
        let left = ProduceError::Cleanup(vec!["the docker volume vol1".into()]);
        let failed = settle_failed_start(&d.dir, &rec, &left, &Fake::default()).await;
        assert_eq!(failed.unwrap_err().kind, FailureKind::NotRemoved);
        // Negative: with the id and volumes already recorded the same error is
        // an ordinary retry, and the confirmed-absent run is forgotten.
        let identified = record(&run('e'), State::Created, Some(&id('f')));
        d.dir.write(&identified).unwrap();
        let retried = settle_failed_start(&d.dir, &identified, &left, &Fake::default()).await;
        assert!(retried.is_ok() && !kept(&d, &run('e')));
        // The names it reported are kept on the record, and a later pass that
        // lists nothing still refuses to forget the run.
        assert_eq!(
            d.dir.read(&run('a')).unwrap().unwrap().leftovers,
            ["the docker volume vol1"]
        );
        let later = reconciled(&d, &Fake::default()).await;
        assert_eq!(later.failures[0].kind, FailureKind::NotRemoved);
        assert!(later.removed.is_empty() && kept(&d, &run('a')));
        // Negative: a start that only failed, with nothing found, owes nothing.
        let refused = ProduceError::Acquire("no image".into());
        assert!(
            settle_failed_start(&d.dir, &rec, &refused, &Fake::default())
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn daemon_discovery_is_bounded_by_the_recovery_deadline() {
        // A daemon that never answers does not hold the recovery past its deadline.
        let error = super::super::discovered(
            Instant::now() + Duration::from_millis(50),
            std::future::pending(),
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("deadline"), "{error}");
    }

    #[tokio::test]
    async fn a_kill_before_the_create_leaves_nothing_and_the_record_goes() {
        let d = Dir::new();
        d.dir
            .write(&record(&run('a'), State::Pending, None))
            .unwrap();
        let rt = Fake::default();
        let done = reconciled(&d, &rt).await;
        assert_eq!(done.removed, vec![run('a')]);
        assert!(done.failures.is_empty() && !kept(&d, &run('a')));
        assert!(rt.removed.borrow().is_empty());
    }

    #[tokio::test]
    async fn a_kill_after_the_create_and_before_the_id_is_found_by_the_run_label() {
        let d = Dir::new();
        d.dir
            .write(&record(&run('a'), State::Pending, None))
            .unwrap();
        let rt = Fake::holding(&container(&run('a'), &id('b')));
        let done = reconciled(&d, &rt).await;
        assert_eq!(done.removed, vec![run('a')]);
        assert_eq!(*rt.removed.borrow(), vec![id('b')]);
    }

    #[tokio::test]
    async fn a_kill_after_the_id_was_recorded_removes_that_id_and_its_record() {
        let d = Dir::new();
        d.dir
            .write(&record(&run('a'), State::Created, Some(&id('b'))))
            .unwrap();
        let rt = Fake::holding(&container(&run('a'), &id('b')));
        let done = reconciled(&d, &rt).await;
        assert_eq!(
            (done.removed, rt.removed.borrow().clone()),
            (vec![run('a')], vec![id('b')])
        );
        assert!(!kept(&d, &run('a')));
    }

    #[tokio::test]
    async fn a_volume_left_after_its_container_is_removed_or_keeps_the_record() {
        let d = Dir::new();
        d.dir
            .write(&record(&run('a'), State::Created, Some(&id('b'))))
            .unwrap();
        // The container is gone and its recorded volume is not.
        let rt = Fake {
            orphans: vec!["vol1".into()],
            ..Fake::default()
        };
        let done = reconciled(&d, &rt).await;
        assert_eq!(
            (done.removed, rt.removed.borrow().clone()),
            (vec![run('a')], vec!["vol1".to_owned()])
        );
        // Negative: a volume the runtime will not drop, or another container
        // now uses, keeps the record, and the latter is never removed.
        let cases = [
            Fake {
                orphans: vec!["vol1".into()],
                cannot_remove_volume: true,
                ..Fake::default()
            },
            Fake {
                volume_users: BTreeMap::from([("vol1".to_owned(), vec![id('f')])]),
                orphans: vec!["vol1".into()],
                ..Fake::default()
            },
        ];
        let kinds = [FailureKind::NotRemoved, FailureKind::Conflict];
        drop(d);
        for (rt, kind) in cases.into_iter().zip(kinds) {
            let d = Dir::new();
            let cid = run('c');
            d.dir
                .write(&record(&cid, State::Created, Some(&id('d'))))
                .unwrap();
            let done = reconciled(&d, &rt).await;
            assert_eq!(done.failures.len(), 1, "{done:?}");
            assert_eq!(done.failures[0].kind, kind);
            assert!(rt.removed.borrow().is_empty() && kept(&d, &cid));
        }
    }

    #[tokio::test]
    async fn a_pending_records_container_and_volumes_are_recorded_before_it_is_removed() {
        let d = Dir::new();
        d.dir
            .write(&record(&run('a'), State::Pending, None))
            .unwrap();
        // The removal dies halfway: the record must now say what it started on.
        let rt = Fake {
            cannot_remove: Some(vec!["the docker volume vol1".into()]),
            ..Fake::holding(&container(&run('a'), &id('b')))
        };
        assert_eq!(reconciled(&d, &rt).await.failures.len(), 1);
        let kept = d.dir.read(&run('a')).unwrap().unwrap();
        assert_eq!(kept.container.id, Some(id('b')));
        assert_eq!(
            (kept.volumes.clone(), kept.state),
            (vec!["vol1".to_owned()], State::Removing)
        );
        // The next pass finds the container gone and still removes its volume.
        let rt = Fake {
            orphans: vec!["vol1".into()],
            ..Fake::default()
        };
        let done = reconciled(&d, &rt).await;
        let said = format!("{done:?}");
        assert_eq!(
            (done.removed, rt.removed.borrow().clone()),
            (vec![run('a')], vec!["vol1".to_owned()]),
            "{said}"
        );
        // Negative: a stranger carrying the label is refused and recorded nowhere.
        d.dir
            .write(&record(&run('c'), State::Pending, None))
            .unwrap();
        let mut stranger = container(&run('c'), &id('d'));
        stranger.name = "someone-elses".into();
        let rt = Fake::holding(&stranger);
        assert_eq!(
            reconciled(&d, &rt).await.failures[0].kind,
            FailureKind::Conflict
        );
        assert_eq!(d.dir.read(&run('c')).unwrap().unwrap().container.id, None);
    }

    #[tokio::test]
    async fn a_live_run_is_left_alone_while_a_dead_one_is_removed_in_the_same_pass() {
        let d = Dir::new();
        let _live = d.dir.lock(&run('1')).unwrap();
        d.dir
            .write(&record(&run('1'), State::Created, Some(&id('1'))))
            .unwrap();
        d.dir
            .write(&record(&run('2'), State::Created, Some(&id('2'))))
            .unwrap();
        let rt = Fake {
            containers: vec![
                container(&run('1'), &id('1')),
                container(&run('2'), &id('2')),
            ],
            ..Fake::default()
        };
        let done = reconciled(&d, &rt).await;
        assert_eq!((done.live, done.removed), (vec![run('1')], vec![run('2')]));
        assert_eq!(*rt.removed.borrow(), vec![id('2')]);
        assert!(kept(&d, &run('1')));
    }

    #[tokio::test]
    async fn a_name_now_held_by_another_container_is_not_the_original_and_is_not_removed() {
        let d = Dir::new();
        d.dir
            .write(&record(&run('a'), State::Created, Some(&id('b'))))
            .unwrap();
        // The original is gone and a different container holds its name.
        let rt = Fake::holding(&container(&run('a'), &id('c')));
        let done = reconciled(&d, &rt).await;
        assert_eq!(
            done.removed,
            vec![run('a')],
            "the original is confirmed absent"
        );
        assert!(rt.removed.borrow().is_empty(), "the stranger is untouched");
        // A pending record finds the stranger by label, and refuses it by name.
        d.dir
            .write(&record(&run('d'), State::Pending, None))
            .unwrap();
        let mut stranger = container(&run('d'), &id('e'));
        stranger.name = "someone-elses".into();
        let rt = Fake::holding(&stranger);
        let done = reconciled(&d, &rt).await;
        assert_eq!(done.failures.len(), 1);
        assert_eq!(done.failures[0].kind, FailureKind::Conflict);
        assert!(rt.removed.borrow().is_empty() && kept(&d, &run('d')));
    }

    #[tokio::test]
    async fn resources_that_disagree_with_the_record_are_never_removed() {
        let conflicts = |change: &dyn Fn(&mut Inspected), users: &[(&str, &[&str])]| {
            let mut found = container(&run('a'), &id('b'));
            change(&mut found);
            let mut rt = Fake::holding(&found);
            rt.volume_users = users
                .iter()
                .map(|(v, u)| ((*v).into(), u.iter().map(|s| (*s).into()).collect()))
                .collect();
            rt
        };
        let cases = [
            conflicts(
                &|c| {
                    c.labels.remove("pbps.resolver");
                },
                &[],
            ),
            conflicts(
                &|c| {
                    c.labels.insert(RUN_LABEL.into(), run('z'));
                },
                &[],
            ),
            conflicts(&|c| c.other_mounts.push("bind".into()), &[]),
            conflicts(&|c| c.volumes.push("not-recorded".into()), &[]),
            conflicts(&|_| {}, &[("vol1", &["f".repeat(64).as_str()])]),
        ];
        for rt in cases {
            let d = Dir::new();
            d.dir
                .write(&record(&run('a'), State::Created, Some(&id('b'))))
                .unwrap();
            let done = reconciled(&d, &rt).await;
            assert_eq!(done.failures.len(), 1, "{done:?}");
            assert_eq!(done.failures[0].kind, FailureKind::Conflict);
            assert!(rt.removed.borrow().is_empty() && kept(&d, &run('a')));
        }
    }

    #[tokio::test]
    async fn an_unreadable_runtime_and_a_timeout_are_not_absence_and_not_each_other() {
        let tries: [(Fake, State, Option<String>, FailureKind); 3] = [
            (
                Fake {
                    list_fails: Some(Fail::Unreadable("denied".into())),
                    ..Fake::default()
                },
                State::Pending,
                None,
                FailureKind::Unreadable,
            ),
            (
                Fake {
                    inspect_fails: Some(Fail::Unreadable("denied".into())),
                    ..Fake::default()
                },
                State::Created,
                Some(id('b')),
                FailureKind::Unreadable,
            ),
            (
                Fake {
                    inspect_fails: Some(Fail::TimedOut("slow".into())),
                    ..Fake::default()
                },
                State::Created,
                Some(id('b')),
                FailureKind::TimedOut,
            ),
        ];
        for (rt, state, cid, kind) in tries {
            let d = Dir::new();
            d.dir
                .write(&record(&run('a'), state, cid.as_deref()))
                .unwrap();
            let done = reconciled(&d, &rt).await;
            assert_eq!(done.failures.len(), 1);
            assert_eq!(done.failures[0].kind, kind);
            assert!(
                done.removed.is_empty() && kept(&d, &run('a')),
                "the record outlives an unconfirmed cleanup"
            );
        }
    }

    #[tokio::test]
    async fn a_removal_the_runtime_does_not_confirm_keeps_the_record_and_names_the_remedy() {
        let d = Dir::new();
        d.dir
            .write(&record(&run('a'), State::Created, Some(&id('b'))))
            .unwrap();
        let rt = Fake {
            cannot_remove: Some(vec!["the docker volume vol1".into()]),
            ..Fake::holding(&container(&run('a'), &id('b')))
        };
        let done = reconciled(&d, &rt).await;
        let failure = &done.failures[0];
        assert_eq!(failure.kind, FailureKind::NotRemoved);
        assert_eq!(failure.resources, vec!["the docker volume vol1".to_owned()]);
        let said = failure.to_string();
        assert!(
            said.contains(&run('a')) && said.contains("docker rm --force --volumes"),
            "{said}"
        );
        assert!(kept(&d, &run('a')));
    }

    #[tokio::test]
    async fn what_cannot_be_proven_is_reported_and_never_removed() {
        let d = Dir::new();
        // The owner's own process with a free lock: a filesystem that ignores locks.
        let mut mine = record(&run('1'), State::Created, Some(&id('1')));
        mine.process = Process::current().unwrap();
        d.dir.write(&mine).unwrap();
        // A record made against another runtime.
        let mut foreign = record(&run('2'), State::Created, Some(&id('2')));
        foreign.daemon.identity = "another daemon".into();
        d.dir.write(&foreign).unwrap();
        // A record that does not parse.
        std::fs::write(d.dir.path().join(format!("{}.json", run('3'))), "{").unwrap();
        let rt = Fake {
            containers: vec![
                container(&run('1'), &id('1')),
                container(&run('2'), &id('2')),
            ],
            ..Fake::default()
        };
        let done = reconciled(&d, &rt).await;
        assert!(done.removed.is_empty() && rt.removed.borrow().is_empty());
        assert_eq!(done.failures.len(), 3, "{done:?}");
        assert!(
            done.failures
                .iter()
                .all(|f| f.kind == FailureKind::Unprovable)
        );
        assert_eq!(d.dir.runs().unwrap().len(), 3);
    }

    #[tokio::test]
    async fn a_passed_deadline_leaves_the_rest_unprocessed_and_reports_it() {
        let d = Dir::new();
        d.dir
            .write(&record(&run('a'), State::Created, Some(&id('b'))))
            .unwrap();
        let rt = Fake::holding(&container(&run('a'), &id('b')));
        let done = reconcile(&d.dir, &rt, &daemon(), Instant::now())
            .await
            .unwrap();
        assert_eq!(done.failures[0].kind, FailureKind::TimedOut);
        assert!(rt.removed.borrow().is_empty() && kept(&d, &run('a')));
    }

    #[tokio::test]
    async fn a_lock_with_no_record_is_nothing_to_confirm() {
        let d = Dir::new();
        drop(d.dir.lock(&run('a')).unwrap());
        assert!(released(&d.dir, &run('a')));
        let done = reconciled(&d, &Fake::default()).await;
        assert_eq!(done.removed, vec![run('a')]);
        assert!(d.dir.runs().unwrap().is_empty());
        // Negative: an empty directory has nothing to do.
        assert_eq!(
            reconciled(&d, &Fake::default()).await,
            super::super::Reconciliation::default()
        );
    }
}

/// Against a real runtime, on the pinned PostgreSQL scratch image.
mod live {
    use super::super::runtime::Docker;
    use super::*;
    use pbps_cli::resolver::server::scratch::container::docker;
    use pbps_config::resolver::PullPolicy;
    use pbps_db::Driver;

    fn image() -> String {
        std::env::var("PBPS_TEST_PG_SCRATCH_IMAGE")
            .expect("PBPS_TEST_PG_SCRATCH_IMAGE must name the pinned PostgreSQL image")
    }

    async fn start(d: &Dir) -> TestScratch<'_> {
        TestScratch::start(&d.dir, Driver::Postgres, &image(), PullPolicy::Never)
            .await
            .unwrap()
    }

    async fn exists(id: &str) -> bool {
        Docker.inspect(id).await.unwrap().is_some()
    }

    /// The owner dies: its lock goes with its process and its process is gone.
    fn kill(d: &Dir, scratch: TestScratch<'_>) -> (String, String) {
        let TestScratch { held, journal } = scratch;
        let record = journal.record.clone();
        held.disown();
        drop(journal);
        let mut dead = record.clone();
        dead.process.pid = GONE;
        d.dir.write(&dead).unwrap();
        (record.run_id, record.container.id.unwrap())
    }

    async fn recover(d: &Dir) -> super::super::Reconciliation {
        super::super::recover(&d.dir, soon()).await.unwrap()
    }

    #[tokio::test]
    #[ignore = "needs docker and the pinned PostgreSQL scratch image"]
    async fn a_run_is_recorded_before_and_with_its_verified_id_and_finish_confirms_it_gone() {
        let d = Dir::new();
        let scratch = start(&d).await;
        pbps_db::Conn::connect(Driver::Postgres, scratch.connection())
            .await
            .unwrap();
        let record = d.dir.read(scratch.run_id()).unwrap().unwrap();
        assert_eq!(record.state, State::Created);
        let cid = record.container.id.clone().unwrap();
        let inspected = docker(&["container", "inspect", "--format", "{{.Id}}", &cid])
            .await
            .unwrap();
        assert_eq!(inspected.trim(), cid, "the recorded id is the runtime's");
        assert!(!record.volumes.is_empty() && exists(&cid).await);
        scratch.finish().await.unwrap();
        assert!(!exists(&cid).await);
        for volume in &record.volumes {
            assert!(
                docker(&["volume", "inspect", volume]).await.is_err(),
                "{volume}"
            );
        }
        assert!(
            d.dir.runs().unwrap().is_empty(),
            "the record goes with the container"
        );
    }

    #[tokio::test]
    #[ignore = "needs docker and the pinned PostgreSQL scratch image"]
    async fn a_killed_run_is_removed_by_the_next_and_a_live_one_is_not() {
        let d = Dir::new();
        let (dead, alive) = (start(&d).await, start(&d).await);
        let alive_id = alive.journal.record.container.id.clone().unwrap();
        let (dead_run, dead_id) = kill(&d, dead);
        assert!(
            exists(&dead_id).await,
            "the killed run's container is still there"
        );
        let done = recover(&d).await;
        assert!(done.failures.is_empty(), "{done:?}");
        assert_eq!(
            (done.removed, done.live),
            (vec![dead_run], vec![alive.run_id().to_owned()])
        );
        assert!(!exists(&dead_id).await && exists(&alive_id).await);
        alive.finish().await.unwrap();
        assert!(d.dir.runs().unwrap().is_empty());
    }

    #[tokio::test]
    #[ignore = "needs docker and the pinned PostgreSQL scratch image"]
    async fn a_failed_removal_fails_the_finish_whatever_the_assertions_did_and_keeps_the_record() {
        struct Refusing;
        impl Runtime for Refusing {
            async fn list(&self, label: &str) -> Result<Vec<String>, Fail> {
                Docker.list(label).await
            }
            async fn inspect(&self, id: &str) -> Result<Option<Inspected>, Fail> {
                Docker.inspect(id).await
            }
            async fn users(&self, volume: &str) -> Result<Vec<String>, Fail> {
                Docker.users(volume).await
            }
            async fn remove_volume(&self, volume: &str) -> Result<(), Vec<String>> {
                Docker.remove_volume(volume).await
            }
            async fn remove(&self, id: &str) -> Result<(), Vec<String>> {
                Err(vec![format!("the container {id}")])
            }
        }
        let d = Dir::new();
        let scratch = start(&d).await;
        let (run_id, cid) = (
            scratch.run_id().to_owned(),
            scratch.journal.record.container.id.clone().unwrap(),
        );
        let failure = scratch.finish_with(&Refusing).await.unwrap_err();
        assert_eq!(failure.kind, FailureKind::NotRemoved);
        assert!(failure.to_string().contains(&run_id));
        assert!(
            exists(&cid).await && kept(&d, &run_id),
            "nothing was confirmed, so nothing was forgotten"
        );
        // The next run finishes what this one could not (its owner is gone).
        let mut dead = d.dir.read(&run_id).unwrap().unwrap();
        dead.process.pid = GONE;
        d.dir.write(&dead).unwrap();
        assert_eq!(recover(&d).await.removed, vec![run_id]);
        assert!(!exists(&cid).await);
    }

    #[tokio::test]
    #[ignore = "needs docker and the pinned PostgreSQL scratch image"]
    async fn a_start_cancelled_before_its_id_is_recorded_leaves_the_container_for_recovery() {
        use pbps_cli::resolver::server::scratch::container::{Held, Owner};
        /// An owner whose record never completes: the start is cancelled there.
        struct Stuck(String, bool);
        impl Owner for Stuck {
            fn token(&self) -> String {
                self.0.clone()
            }
            fn labels(&self) -> Vec<String> {
                vec![format!("{RUN_LABEL}={}", self.0)]
            }
            fn recoverable(&self) -> bool {
                self.1
            }
            async fn before_create(&mut self) -> Result<(), String> {
                Ok(())
            }
            async fn created(&mut self, _: &str) -> Result<(), String> {
                std::future::pending().await
            }
        }
        /// Removes the containers by their exact names, whatever the test found.
        struct Made(Vec<String>);
        impl Drop for Made {
            fn drop(&mut self) {
                for name in &self.0 {
                    let _ = std::process::Command::new("docker")
                        .args(["rm", "--force", "--volumes", name])
                        .output();
                }
            }
        }
        let _serial = Dir::new();
        let (recorded, anonymous) = (record::new_run_id(), record::new_run_id());
        let _made = Made(vec![
            format!("pbps-scratch-{recorded}"),
            format!("pbps-scratch-{anonymous}"),
        ]);
        for (token, recoverable) in [(&recorded, true), (&anonymous, false)] {
            let (image, mut owner) = (image(), Stuck(token.clone(), recoverable));
            let start = Held::start_owned(Driver::Postgres, &image, PullPolicy::Never, &mut owner);
            assert!(
                tokio::time::timeout(Duration::from_secs(20), start)
                    .await
                    .is_err()
            );
        }
        // The drop fallback is a spawned `docker rm`: give it the time it needs.
        tokio::time::sleep(Duration::from_secs(8)).await;
        let listed = |token: &String| {
            let name = format!("pbps-scratch-{token}");
            async move {
                !Docker
                    .list(&format!(
                        "{RUN_LABEL}={}",
                        name.trim_start_matches("pbps-scratch-")
                    ))
                    .await
                    .unwrap()
                    .is_empty()
            }
        };
        assert!(
            listed(&recorded).await,
            "a recoverable start's container waits for recovery"
        );
        assert!(
            !listed(&anonymous).await,
            "the resolver's own start still removes its container"
        );
    }

    #[path = "swap2505.rs"]
    mod swap2505;

    #[tokio::test]
    #[ignore = "needs docker and the pinned PostgreSQL scratch image"]
    async fn a_start_failing_after_its_id_is_recorded_removes_that_container_not_the_one_now_holding_its_name()
     {
        if std::env::var_os("PBPS_SWAP2505_CHILD").is_some() {
            swap2505::exercise().await;
        } else {
            swap2505::controls().await;
        }
    }

    #[tokio::test]
    #[ignore = "needs docker and the pinned PostgreSQL scratch image"]
    async fn a_start_whose_record_fails_leaves_the_container_to_the_journaled_removal() {
        use pbps_cli::resolver::server::scratch::container::{Held, Owner};
        /// An owner that cannot record the container it was just given.
        struct Refusing(String, bool);
        impl Owner for Refusing {
            fn token(&self) -> String {
                self.0.clone()
            }
            fn labels(&self) -> Vec<String> {
                vec![format!("{RUN_LABEL}={}", self.0)]
            }
            fn recoverable(&self) -> bool {
                self.1
            }
            async fn before_create(&mut self) -> Result<(), String> {
                Ok(())
            }
            async fn created(&mut self, _: &str) -> Result<(), String> {
                Err("the record could not be written".into())
            }
        }
        struct Made(Vec<String>);
        impl Drop for Made {
            fn drop(&mut self) {
                for name in &self.0 {
                    let _ = std::process::Command::new("docker")
                        .args(["rm", "--force", "--volumes", name])
                        .output();
                }
            }
        }
        let _serial = Dir::new();
        let (recorded, anonymous) = (record::new_run_id(), record::new_run_id());
        let _made = Made(vec![
            format!("pbps-scratch-{recorded}"),
            format!("pbps-scratch-{anonymous}"),
        ]);
        for (token, recoverable) in [(&recorded, true), (&anonymous, false)] {
            let mut owner = Refusing(token.clone(), recoverable);
            let started =
                Held::start_owned(Driver::Postgres, &image(), PullPolicy::Never, &mut owner).await;
            assert!(started.is_err());
        }
        let listed = |token: &String| {
            let label = format!("{RUN_LABEL}={token}");
            async move { !Docker.list(&label).await.unwrap().is_empty() }
        };
        assert!(
            listed(&recorded).await,
            "the owner's removal, not the resolver's, takes it"
        );
        assert!(
            !listed(&anonymous).await,
            "the resolver's own start still removes at once"
        );
    }

    #[tokio::test]
    #[ignore = "needs docker and the pinned PostgreSQL scratch image"]
    async fn a_pending_record_finds_its_container_by_label_and_a_stranger_by_name_is_kept() {
        /// Removes the two containers by their exact names, whatever the test found.
        struct Made(Vec<String>);
        impl Drop for Made {
            fn drop(&mut self) {
                for name in &self.0 {
                    let _ = std::process::Command::new("docker")
                        .args(["rm", "--force", "--volumes", name])
                        .output();
                }
            }
        }
        let d = Dir::new();
        let (run_id, other) = (record::new_run_id(), record::new_run_id());
        let _made = Made(vec![
            format!("pbps-test-ours-{run_id}"),
            format!("pbps-test-stranger-{other}"),
        ]);
        let create = |name: &str, label_run: &str| {
            let (name, label) = (name.to_owned(), format!("{RUN_LABEL}={label_run}"));
            let image = image();
            async move {
                docker(&[
                    "create", "--pull", "never", "--name", &name, "--label", &label, &image,
                ])
                .await
                .unwrap()
                .trim()
                .to_owned()
            }
        };
        // The run was killed after `docker run` and before its id was recorded.
        let here = super::super::runtime::current().await.unwrap();
        let mut pending = record(&run_id, State::Pending, None);
        pending.volumes.clear();
        pending.daemon = here.clone();
        // Neither these containers nor their records carry the resolver's
        // scratch label, so no resolver sweep on this host can see them.
        pending.container.name = format!("pbps-test-ours-{run_id}");
        pending.labels.remove("pbps.resolver");
        let ours = create(&format!("pbps-test-ours-{run_id}"), &run_id).await;
        d.dir.write(&pending).unwrap();
        // Another run's record whose label a container of another name carries.
        let mut second = record(&other, State::Pending, None);
        second.volumes.clear();
        second.daemon = here;
        second.labels.remove("pbps.resolver");
        let stranger = create(&format!("pbps-test-stranger-{other}"), &other).await;
        d.dir.write(&second).unwrap();
        let done = recover(&d).await;
        assert_eq!(done.removed, vec![run_id], "{done:?}");
        assert_eq!(done.failures.len(), 1, "{done:?}");
        assert_eq!(done.failures[0].kind, FailureKind::Conflict);
        assert!(!exists(&ours).await && exists(&stranger).await);
    }
}

#[tokio::test]
async fn d2a1165_the_private_key_survives_unconfirmed_cleanup_and_goes_with_its_dead_run() {
    let dir = Dir::new();
    let run_id = run('a');
    let cid = id('a');
    let record = record(&run_id, State::Created, Some(&cid));
    let lock = dir.dir.lock(&run_id).unwrap();
    dir.dir.write(&record).unwrap();
    drop(lock);
    assert!(released(&dir.dir, &run_id));
    let key = dir.dir.file(&run_id, "key");
    std::fs::write(&key, b"private key sentinel").unwrap();
    let held = container(&run_id, &cid);
    let refusing = Fake {
        cannot_remove: Some(vec![cid]),
        ..Fake::holding(&held)
    };
    let refused = reconciled(&dir, &refusing).await;
    assert_eq!(refused.failures.len(), 1, "{refused:?}");
    assert_eq!(std::fs::read(&key).unwrap(), b"private key sentinel");
    assert!(kept(&dir, &run_id));
    // A sibling test's fork can retain the first recovery's file lock until exec.
    assert!(released(&dir.dir, &run_id));
    let cleaned = reconciled(&dir, &Fake::holding(&held)).await;
    assert_eq!(cleaned.removed, vec![run_id], "{cleaned:?}");
    assert!(!key.exists());
}

#[tokio::test]
async fn d2a1165_interrupted_start_keeps_pending_recovery_evidence_through_empty_and_late_listings()
{
    use pbps_cli::resolver::server::ProduceError;
    for (cancelled, token) in [(false, 'a'), (true, 'b')] {
        let d = Dir::new();
        let rec = record(&run(token), State::Pending, None);
        d.dir.write(&rec).unwrap();
        let cancellation = async move {
            if cancelled {
                Ok(())
            } else {
                std::future::pending::<std::io::Result<()>>().await
            }
        };
        let end = tokio::time::Instant::now()
            + if cancelled {
                Duration::from_secs(60)
            } else {
                Duration::from_millis(1)
            };
        let error = super::bounded_start(
            std::future::pending::<Result<super::Held, ProduceError>>(),
            end,
            cancellation,
            &rec.container.name,
        )
        .await
        .err()
        .unwrap();
        let cleanup = settle_failed_start(&d.dir, &rec, &error, &Fake::default()).await;
        assert!(
            cleanup.is_err(),
            "an interrupted runtime request was forgotten on an empty listing"
        );
        let kept_record = d.dir.read(&rec.run_id).unwrap().unwrap();
        assert!(!kept_record.leftovers.is_empty());
        let late = Fake::holding(&container(&rec.run_id, &id('c')));
        let later = reconciled(&d, &late).await;
        assert!(
            later.removed.is_empty()
                && !later.failures.is_empty()
                && late.removed.borrow().is_empty()
        );
        assert!(kept(&d, &rec.run_id));
        // A conclusively absent identified container is still cleanable.
        let known = record(&run('d'), State::Created, Some(&id('e')));
        d.dir.write(&known).unwrap();
        assert!(
            settle_failed_start(&d.dir, &known, &error, &Fake::default())
                .await
                .is_ok()
        );
        // Without the before-create record, no request was issued.
        let untouched = record(&run('f'), State::Pending, None);
        assert!(
            settle_failed_start(&d.dir, &untouched, &error, &Fake::default())
                .await
                .is_ok()
        );
        assert!(!kept(&d, &untouched.run_id));
    }
    // An unreadable record cannot take the provably unissued-start path.
    let d = Dir::new();
    let rec = record(&run('c'), State::Pending, None);
    let path = d.dir.file(&rec.run_id, "json");
    std::fs::write(&path, b"not a run record").unwrap();
    let unknown = ProduceError::Cleanup(vec![rec.container.name.clone()]);
    let error = settle_failed_start(&d.dir, &rec, &unknown, &Fake::default())
        .await
        .unwrap_err();
    assert_eq!(error.kind, FailureKind::Unprovable);
    assert_eq!(std::fs::read(path).unwrap(), b"not a run record");
}
