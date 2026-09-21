//! The lock files of ADR-0015 decision 5's step 0, taken the way `git` takes
//! them and published the way nothing else here is allowed to write.
//!
//! `git` refuses a lock file whatever is inside it (**measured**: with
//! `HEAD.lock` and a branch's lock holding a line of text, `symbolic-ref` and
//! `update-ref` both failed with `File exists`, while reading the ref still
//! worked), and those files are empty in `git`'s own use. So their content is
//! free for the UI, and the record's id written into each one is the only way
//! to tell its own leftover lock — from a crash — from a lock another `git` is
//! holding right now.
//!
//! `<index>.lock` is deliberately not one of them: it is a *copy of the index*
//! and is read and installed as one, and a byte added to it stops being an
//! index at all (**measured**: `update-index` refused a copy with the id
//! appended, `index uses … extension, which we do not understand`). Its
//! ownership is established by hash instead.

use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};

use super::fsx::Dir;
use super::git::{Failure, Git};

/// Why a lock could not be taken. Each is a refusal with a place in it, never
/// a bare error: the user has to be able to see which file stands in the way.
#[derive(Debug)]
pub enum LockRefusal {
    /// The file is already there and is not this record's. Another `git` is
    /// mid-operation, or another compose is running.
    Held {
        path: PathBuf,
        holding: String,
    },
    /// `git` could not say where the lock belongs.
    Unlocatable {
        reference: String,
        detail: String,
    },
    Io {
        path: PathBuf,
        detail: String,
    },
}

impl std::fmt::Display for LockRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Held { path, holding } => write!(
                f,
                "{} already exists{}; another git command is running in this checkout",
                path.display(),
                if holding.is_empty() {
                    String::new()
                } else {
                    format!(" and holds `{holding}`")
                }
            ),
            Self::Unlocatable { reference, detail } => {
                write!(f, "git could not say where {reference} is locked: {detail}")
            }
            Self::Io { path, detail } => write!(f, "could not lock {}: {detail}", path.display()),
        }
    }
}

/// A lock this process holds.
///
/// Dropping one does *not* release it: a lock is released by the step that
/// decides the compose is over, and a failure to release is something the page
/// has to be told about, which a `Drop` cannot do.
#[derive(Debug)]
pub struct Held {
    path: PathBuf,
    directory: Dir,
    name: OsString,
    /// True where this process created the file, false where it reclaimed one
    /// a crashed compose left holding the same record id.
    created: bool,
}

impl Held {
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn reclaimed(&self) -> bool {
        !self.created
    }

    /// Released whatever the outcome: the process that took it is gone or
    /// finished, so it holds nothing together and only stops every other
    /// `git` on that ref.
    pub fn release(self) -> io::Result<()> {
        self.directory.unlink(&self.name)?;
        self.directory.sync()
    }

    /// Let go of the lock *without* removing the file, for the one case where
    /// `git` itself takes it over: `update-ref` takes `HEAD`'s lock and the
    /// branch's during its transaction, so the UI gives `HEAD.lock` up across
    /// the whole transaction rather than holding a file `git` needs.
    pub fn hand_over(self) {
        // Nothing to do, and that is the point: `Held` has no `Drop`, so
        // letting it go closes this process's handles and leaves the file
        // exactly where `git` now needs it.
    }
}

/// Take a lock file beside the place `git` would put it.
///
/// Published by `link()` from a temporary made whole beside it, not created
/// and then filled in: creating the file first and identifying it afterwards
/// would leave a crash in that gap holding a lock nobody can claim — the UI
/// would read it as another `git`'s and refuse forever, and so would every
/// ordinary `git` command.
pub fn take(
    root: &Path,
    lock_file: &Path,
    contents: &[u8],
    record_id: &str,
) -> Result<Held, LockRefusal> {
    let absolute = if lock_file.is_absolute() {
        lock_file.to_path_buf()
    } else {
        root.join(lock_file)
    };
    let parent = absolute.parent().unwrap_or(root);
    // `git` creates these directories itself when it writes a ref that is
    // currently packed; a lock for `refs/heads/feature/x` has nowhere to go
    // until `refs/heads/feature/` exists.
    std::fs::create_dir_all(parent).map_err(|e| LockRefusal::Io {
        path: parent.to_path_buf(),
        detail: e.to_string(),
    })?;
    let directory = Dir::open_root(parent).map_err(|e| LockRefusal::Io {
        path: parent.to_path_buf(),
        detail: e.to_string(),
    })?;
    let name = absolute
        .file_name()
        .ok_or_else(|| LockRefusal::Io {
            path: absolute.clone(),
            detail: "the lock has no file name".to_owned(),
        })?
        .to_owned();

    // A lock already there may be this record's own, left by a crash. Reading
    // it is how the UI tells its leftover from a running `git`'s.
    if let Ok(Some(_)) = directory.look(&name) {
        let existing = directory
            .open_file(&name)
            .and_then(|file| file.read())
            .unwrap_or_default();
        return if existing == contents && !record_id.is_empty() {
            Ok(Held {
                path: absolute,
                directory,
                name,
                created: false,
            })
        } else {
            Err(LockRefusal::Held {
                path: absolute,
                holding: String::from_utf8_lossy(&existing).trim().to_owned(),
            })
        };
    }

    let temporary = OsString::from(format!(
        ".pbps-ui-lock-{record_id}-{}",
        name.to_string_lossy()
    ));
    let _ = directory.unlink(&temporary);
    let file = directory
        .create_new(&temporary, 0o600)
        .map_err(|e| LockRefusal::Io {
            path: absolute.clone(),
            detail: e.to_string(),
        })?;
    let complete = (|| {
        file.write_all(contents)?;
        file.flush()
    })();
    drop(file);
    if let Err(e) = complete {
        let _ = directory.unlink(&temporary);
        return Err(LockRefusal::Io {
            path: absolute,
            detail: e.to_string(),
        });
    }

    let published = directory.link(&temporary, &name);
    let _ = directory.unlink(&temporary);
    match published {
        Ok(()) => {
            directory.sync().map_err(|e| LockRefusal::Io {
                path: absolute.clone(),
                detail: e.to_string(),
            })?;
            Ok(Held {
                path: absolute,
                directory,
                name,
                created: true,
            })
        }
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Err(LockRefusal::Held {
            path: absolute,
            holding: String::new(),
        }),
        Err(e) => Err(LockRefusal::Io {
            path: absolute,
            detail: e.to_string(),
        }),
    }
}

/// Where `git` would put the lock for a ref or for the index.
///
/// Asked of `git` rather than assembled, because not every ref is shared: a
/// per-worktree one — `refs/worktree/*`, `refs/bisect/*`, `refs/rewritten/*` —
/// lives under the worktree's own directory, and the common directory would
/// name a file nothing locks (**measured** in a linked worktree on git 2.43:
/// `--git-path refs/worktree/foo` answered under `worktrees/<name>/` where
/// `--git-path refs/heads/<branch>` answered under the common directory).
pub fn lock_file_for(git: &Git, what: &str) -> Result<PathBuf, LockRefusal> {
    let answer = git
        .run(&["rev-parse", "--git-path", what])
        .map_err(|e: Failure| LockRefusal::Unlocatable {
            reference: what.to_owned(),
            detail: e.to_string(),
        })?;
    if !answer.ok() {
        return Err(LockRefusal::Unlocatable {
            reference: what.to_owned(),
            detail: String::from_utf8_lossy(&answer.stderr).trim().to_owned(),
        });
    }
    let line = answer.line().map_err(|e| LockRefusal::Unlocatable {
        reference: what.to_owned(),
        detail: e.to_string(),
    })?;
    Ok(PathBuf::from(format!("{line}.lock")))
}

/// The index's own path, which is not a lock: step 0 copies the index to
/// `<index>.lock` and step 6 installs it by renaming it back.
///
/// Answered absolute. `git rev-parse --git-path` answers relative to the
/// worktree root in the main worktree and absolute in a linked one
/// (**measured**), and the UI is not standing in the worktree root — a
/// relative answer used as a path would name a file beside whatever directory
/// `pbps ui` was started from.
pub fn index_file(git: &Git) -> Result<PathBuf, LockRefusal> {
    let answer = git
        .run(&["rev-parse", "--git-path", "index"])
        .map_err(|e| LockRefusal::Unlocatable {
            reference: "the index".to_owned(),
            detail: e.to_string(),
        })?;
    let line = answer.line().map_err(|e| LockRefusal::Unlocatable {
        reference: "the index".to_owned(),
        detail: e.to_string(),
    })?;
    let path = PathBuf::from(line);
    Ok(if path.is_absolute() {
        path
    } else {
        git.root().join(path)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compose::record::random_name;

    fn scratch(name: &str) -> PathBuf {
        let directory = std::env::temp_dir().join(format!(
            "pbps-lock-{name}-{}-{}",
            std::process::id(),
            random_name().unwrap()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        directory
    }

    #[test]
    fn a_lock_is_never_seen_in_a_state_the_record_cannot_account_for() {
        // Published by `link()` from a whole temporary. The property under
        // test is that the name, once it exists, already holds the id: a
        // crash between creating the file and filling it in would leave a
        // lock nobody can claim, and every ordinary `git` would refuse too.
        let root = scratch("publish");
        let lock = root.join("HEAD.lock");
        let held = take(&root, Path::new("HEAD.lock"), b"record-1\n", "record-1").unwrap();

        assert_eq!(std::fs::read(&lock).unwrap(), b"record-1\n");
        assert!(held.path().ends_with("HEAD.lock"));
        assert!(!held.reclaimed());
        // The temporary the link was made from is gone; the lock is one name.
        let leftovers: Vec<_> = std::fs::read_dir(&root)
            .unwrap()
            .filter_map(Result::ok)
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with(".pbps-ui-lock-"))
            .collect();
        assert!(leftovers.is_empty(), "left {leftovers:?}");

        held.release().unwrap();
        assert!(!lock.exists());
    }

    #[test]
    fn a_lock_another_git_is_holding_is_refused_and_shown() {
        let root = scratch("held");
        std::fs::write(root.join("HEAD.lock"), b"").unwrap();

        let refusal = take(&root, Path::new("HEAD.lock"), b"mine\n", "mine").unwrap_err();
        match refusal {
            LockRefusal::Held { path, holding } => {
                assert!(path.ends_with("HEAD.lock"));
                assert_eq!(holding, "", "git's own locks are empty");
            }
            other @ (LockRefusal::Unlocatable { .. } | LockRefusal::Io { .. }) => {
                panic!("expected a held lock, got {other}")
            }
        }
        assert!(root.join("HEAD.lock").exists(), "and it is left alone");
    }

    #[test]
    fn a_lock_a_crashed_compose_left_holding_this_record_is_reclaimed_not_refused() {
        // The recovery case: the locks a crash left behind are still on disk,
        // so recovery does not create them again — it continues only where
        // each is either absent, or already holds this record's id.
        let root = scratch("reclaim");
        std::fs::write(root.join("HEAD.lock"), b"record-7\n").unwrap();

        let held = take(&root, Path::new("HEAD.lock"), b"record-7\n", "record-7").unwrap();
        assert!(held.reclaimed(), "it is used as it stands");
        held.release().unwrap();
        assert!(!root.join("HEAD.lock").exists());

        // And a lock holding some other record's id is a running `git`'s.
        std::fs::write(root.join("HEAD.lock"), b"record-9\n").unwrap();
        assert!(matches!(
            take(&root, Path::new("HEAD.lock"), b"record-7\n", "record-7").unwrap_err(),
            LockRefusal::Held { .. }
        ));
    }

    #[test]
    fn a_lock_for_a_packed_ref_gets_the_directory_git_would_have_made() {
        // A ref that is currently packed has no loose file and may have no
        // directory either; `git` creates it when it writes the ref, and a
        // lock that did not would refuse a compose on any branch with a slash
        // in its name.
        let root = scratch("packed");
        let held = take(
            &root,
            Path::new(".git/refs/heads/feature/x.lock"),
            b"id\n",
            "id",
        )
        .unwrap();
        assert!(root.join(".git/refs/heads/feature/x.lock").exists());
        held.release().unwrap();
    }
}
