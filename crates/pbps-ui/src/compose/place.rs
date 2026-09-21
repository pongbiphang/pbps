//! Step 2 of ADR-0015 decision 5: putting the intent command's files into the
//! working tree so that a refusal anywhere after it can put everything back.
//!
//! Nothing here overwrites. A path that exists is *exchanged* with a
//! temporary beside it, so that what came out is still a file, under a name,
//! to be inspected and then kept; a path that does not exist is placed with
//! `link()`, which refuses with `EEXIST` if something made the name first. The
//! one name this module ever unlinks is the temporary its own `link()` was
//! made from, and only after the branch has moved. Everything else it takes
//! away, it takes by exchange or rename — because after an editor saves by
//! writing a temporary and renaming it over, the path holds *that* inode with
//! a link count of one, and an unlink would take the editor's only name with
//! it.
//!
//! Since an intent may edit more than one file, a refusal that left some of
//! them replaced would be a partial edit nobody asked for. So the undo is in
//! reverse order and covers every path placed before the one that failed.

use std::ffi::OsString;
use std::os::unix::ffi::OsStrExt as _;

use super::attributes::{self, AttributeRefusal};
use super::fsx::{Dir, FileAt, Metadata, WalkRefusal};
use super::git::Git;
use super::repo_path::RepoPath;

/// One file the intent command produced, and what the page was shown of it.
#[derive(Debug, Clone)]
pub struct Wanted {
    pub path: RepoPath,
    pub bytes: Vec<u8>,
    /// What the preview saw at this path, where it saw anything at all.
    ///
    /// Two `Option`s and not one, because "the preview expected nothing here"
    /// and "the preview said nothing about this path" are different
    /// instructions: collapsing them lets a file created after the earlier
    /// check be accepted as an existing one and exchanged away, and the commit
    /// then holds bytes that replaced a creation nobody previewed. The locks
    /// stop `git`, not an editor.
    pub shown: Option<Option<String>>,
}

#[derive(Debug)]
pub enum PlaceRefusal {
    Walk(WalkRefusal),
    /// The leaf is a link, or something that is not a regular file. `git
    /// hash-object` follows a link and hashes the target's content, so a
    /// linked declaration would commit its destination's bytes.
    NotARegularFile {
        path: String,
    },
    /// An editor saved between step 1's check and the placement.
    Changed {
        path: String,
    },
    /// A read-back disagreed with what was written or copied a moment
    /// earlier, which means something changed it in that gap and the copy is
    /// already stale.
    Readback {
        path: String,
        what: String,
    },
    /// The handle and a fresh no-link lookup no longer name one directory: an
    /// ancestor was renamed away and another put in its place.
    DirectoryMoved {
        path: String,
    },
    /// The name appeared in the window between step 1 and the `link()`.
    Appeared {
        path: String,
    },
    Attribute(AttributeRefusal),
    Io {
        path: String,
        detail: String,
    },
}

impl std::fmt::Display for PlaceRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Walk(refusal) => write!(f, "{refusal}"),
            Self::NotARegularFile { path } => {
                write!(f, "`{path}` is not a regular file in the working tree")
            }
            Self::Changed { path } => write!(
                f,
                "`{path}` changed since it was read; something else has saved it"
            ),
            Self::Readback { path, what } => write!(
                f,
                "`{path}` did not read back as it was written ({what}); \
                 something else changed it in that moment"
            ),
            Self::DirectoryMoved { path } => write!(
                f,
                "the directory holding `{path}` was replaced while the compose ran"
            ),
            Self::Appeared { path } => write!(
                f,
                "`{path}` was created by something else while the compose ran"
            ),
            Self::Attribute(refusal) => write!(f, "{refusal}"),
            Self::Io { path, detail } => write!(f, "could not place `{path}`: {detail}"),
        }
    }
}

impl From<WalkRefusal> for PlaceRefusal {
    fn from(refusal: WalkRefusal) -> Self {
        Self::Walk(refusal)
    }
}

impl From<AttributeRefusal> for PlaceRefusal {
    fn from(refusal: AttributeRefusal) -> Self {
        Self::Attribute(refusal)
    }
}

/// What the record needs to know about one placement *before* it happens.
#[derive(Debug, Clone)]
pub struct Intent {
    pub path: String,
    pub temporary: String,
    pub original: Option<(String, u64, u64)>,
    pub replacement: (String, u64, u64),
}

/// A file this process has put in the working tree.
#[derive(Debug)]
struct Done {
    path: RepoPath,
    parent: Dir,
    leaf: OsString,
    temporary: OsString,
    /// The descriptor the temporary was written and read back through, which
    /// the exchange leaves bound to the inode now at the path.
    handle: FileAt,
    /// `true` where `link()` created the name, `false` where an exchange
    /// replaced an existing file.
    created: bool,
    previous: Option<Previous>,
    /// For a path the tip does not hold: the execute bit of the file step 2
    /// found there, which is the mode `git add` would have recorded.
    executable: bool,
    undone: bool,
}

#[derive(Debug, Clone)]
struct Previous {
    metadata: Metadata,
    blob: String,
    /// The bytes read through the file's own handle in step 1. Kept because
    /// the attribute check needs *both* sides: an attribute that leaves the
    /// old content alone can still act on the new, and one that transforms
    /// the old would leave the path modified the moment it is committed.
    bytes: Vec<u8>,
}

/// The placements of one compose, in the order they were made.
#[derive(Debug)]
pub struct Placement {
    root: Dir,
    done: Vec<Done>,
}

/// What became of one path when the placement was undone.
#[derive(Debug, PartialEq, Eq)]
pub enum Undone {
    /// The exchange was reversed; the path holds what it held.
    Restored { path: String },
    /// A `link()`ed name was renamed away rather than unlinked, because the
    /// entry may be an editor's inode by now.
    RenamedAway { path: String, to: String },
    /// Nothing could be done for this path, and it is reported rather than
    /// forced.
    Reported { path: String, detail: String },
}

impl Placement {
    pub fn new(root: Dir) -> Self {
        Self {
            root,
            done: Vec::new(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.done.is_empty()
    }

    /// What the `placing` phase records for this path, produced *before* the
    /// exchange: the record cannot say afterwards whether it happened, so it
    /// carries what identifies each of the two files.
    pub fn intend(
        &self,
        git: &Git,
        wanted: &Wanted,
        suffix: &str,
    ) -> Result<(Intent, Prepared), PlaceRefusal> {
        let (parent, leaf) = self.root.walk_to_parent(&wanted.path)?;
        let temporary = OsString::from(temporary_name(&wanted.path, suffix));
        let existing = parent.look(&leaf).map_err(|e| PlaceRefusal::Io {
            path: wanted.path.to_string(),
            detail: e.to_string(),
        })?;
        // Expected absent, and something is there now.
        if matches!(wanted.shown, Some(None)) && existing.is_some() {
            return Err(PlaceRefusal::Appeared {
                path: wanted.path.to_string(),
            });
        }
        // Expected content, and the path is gone.
        if matches!(wanted.shown, Some(Some(_))) && existing.is_none() {
            return Err(PlaceRefusal::Changed {
                path: wanted.path.to_string(),
            });
        }
        let previous = match existing {
            None => None,
            Some(look) if look.is_link || !look.is_regular => {
                return Err(PlaceRefusal::NotARegularFile {
                    path: wanted.path.to_string(),
                });
            }
            Some(_) => {
                let file = parent.open_file(&leaf).map_err(|e| PlaceRefusal::Io {
                    path: wanted.path.to_string(),
                    detail: e.to_string(),
                })?;
                let bytes = file.read().map_err(|e| PlaceRefusal::Io {
                    path: wanted.path.to_string(),
                    detail: e.to_string(),
                })?;
                let blob = attributes::hash(git, &bytes)?;
                if let Some(Some(shown)) = &wanted.shown
                    && *shown != blob
                {
                    return Err(PlaceRefusal::Changed {
                        path: wanted.path.to_string(),
                    });
                }
                let metadata = file.metadata().map_err(|e| PlaceRefusal::Io {
                    path: wanted.path.to_string(),
                    detail: e.to_string(),
                })?;
                let identity = file.identity().map_err(|e| PlaceRefusal::Io {
                    path: wanted.path.to_string(),
                    detail: e.to_string(),
                })?;
                Some((
                    Previous {
                        metadata,
                        blob,
                        bytes,
                    },
                    identity,
                ))
            }
        };

        // The replacement is made whole under the temporary name before the
        // record names it, so the record's two identities are both real.
        let _ = parent.unlink(&temporary);
        let handle = parent
            .create_new(&temporary, previous.as_ref().map_or(0o666, |_| 0o600))
            .map_err(|e| PlaceRefusal::Io {
                path: wanted.path.to_string(),
                detail: e.to_string(),
            })?;
        let written = (|| {
            handle.write_all(&wanted.bytes)?;
            handle.flush()
        })();
        if let Err(e) = written {
            let _ = parent.unlink(&temporary);
            return Err(PlaceRefusal::Io {
                path: wanted.path.to_string(),
                detail: e.to_string(),
            });
        }
        if let Some((previous, _)) = &previous {
            handle
                .apply(&previous.metadata)
                .map_err(|e| PlaceRefusal::Io {
                    path: wanted.path.to_string(),
                    detail: e.to_string(),
                })?;
            let copied = handle.metadata().map_err(|e| PlaceRefusal::Io {
                path: wanted.path.to_string(),
                detail: e.to_string(),
            })?;
            if copied != previous.metadata {
                let _ = parent.unlink(&temporary);
                return Err(PlaceRefusal::Readback {
                    path: wanted.path.to_string(),
                    what: "the copied metadata".to_owned(),
                });
            }
        }
        let replacement_blob = attributes::hash(git, &wanted.bytes)?;
        let replacement_identity = handle.identity().map_err(|e| PlaceRefusal::Io {
            path: wanted.path.to_string(),
            detail: e.to_string(),
        })?;
        let executable = previous
            .as_ref()
            .map(|(previous, _)| previous.metadata.executable())
            .unwrap_or(false);

        Ok((
            Intent {
                path: wanted.path.to_string(),
                temporary: temporary.to_string_lossy().into_owned(),
                original: previous.as_ref().map(|(previous, identity)| {
                    (previous.blob.clone(), identity.device, identity.inode)
                }),
                replacement: (
                    replacement_blob,
                    replacement_identity.device,
                    replacement_identity.inode,
                ),
            },
            Prepared {
                path: wanted.path.clone(),
                parent,
                leaf,
                temporary,
                handle,
                previous: previous.map(|(previous, _)| previous),
                executable,
                bytes: wanted.bytes.clone(),
            },
        ))
    }

    /// Put the prepared replacement at the path.
    ///
    /// Everything before this call is reversible by removing one temporary
    /// file; everything after it is reversible only by the undo below, which
    /// is why the `placing` phase is published in between.
    pub fn install(
        &mut self,
        git: &Git,
        tip: &str,
        prepared: Prepared,
    ) -> Result<(), PlaceRefusal> {
        let Prepared {
            path,
            parent,
            leaf,
            temporary,
            handle,
            previous,
            executable,
            bytes,
        } = prepared;
        let created = previous.is_none();
        let outcome = if created {
            parent.link(&temporary, &leaf).map_err(|e| {
                if e.kind() == std::io::ErrorKind::AlreadyExists {
                    PlaceRefusal::Appeared {
                        path: path.to_string(),
                    }
                } else {
                    PlaceRefusal::Io {
                        path: path.to_string(),
                        detail: e.to_string(),
                    }
                }
            })
        } else {
            parent
                .exchange(&leaf, &temporary)
                .map_err(|e| PlaceRefusal::Io {
                    path: path.to_string(),
                    detail: e.to_string(),
                })
        };
        if let Err(refusal) = outcome {
            let _ = parent.unlink(&temporary);
            return Err(refusal);
        }

        let done = Done {
            path: path.clone(),
            parent,
            leaf,
            temporary,
            handle,
            created,
            previous,
            executable,
            undone: false,
        };
        self.done.push(done);

        // From here a failure undoes this path too, so the checks come after
        // the push and report through the ordinary path.
        let placed = self.done.last().expect("just pushed");
        if !placed
            .parent
            .still_at(&self.root, &placed.path)
            .map_err(PlaceRefusal::Walk)?
        {
            return Err(PlaceRefusal::DirectoryMoved {
                path: path.to_string(),
            });
        }
        // What came out, and what went in, each read back through a handle
        // rather than by reopening a name.
        if let Some(previous) = &placed.previous {
            let came_out = placed
                .parent
                .open_file(&placed.temporary)
                .and_then(|file| Ok((file.read()?, file.metadata()?)))
                .map_err(|e| PlaceRefusal::Io {
                    path: path.to_string(),
                    detail: e.to_string(),
                })?;
            if attributes::hash(git, &came_out.0)? != previous.blob {
                return Err(PlaceRefusal::Changed {
                    path: path.to_string(),
                });
            }
            if came_out.1 != previous.metadata {
                return Err(PlaceRefusal::Readback {
                    path: path.to_string(),
                    what: "the metadata of the file that came out".to_owned(),
                });
            }
        }
        let went_in = placed.handle.read().map_err(|e| PlaceRefusal::Io {
            path: path.to_string(),
            detail: e.to_string(),
        })?;
        if went_in != bytes {
            return Err(PlaceRefusal::Readback {
                path: path.to_string(),
                what: "the bytes at the path".to_owned(),
            });
        }
        // Both attribute questions answer by path name — `--path` and
        // `check-attr` read the `.gitattributes` files along the name as it is
        // at that moment — so they are asked here, after the placement and in
        // the same instant as the identity confirmation, and a failure rolls
        // this path back like any other. The pair of hashes is taken of the
        // file's *old* content and of the replacement, because an attribute
        // that leaves one alone can still act on the other.
        let current = placed
            .previous
            .as_ref()
            .map_or(bytes.as_slice(), |previous| previous.bytes.as_slice());
        attributes::refuse_transformations(
            git,
            path.to_text().ok_or_else(|| PlaceRefusal::Io {
                path: path.to_string(),
                detail: "this path's name is not text git can be given".to_owned(),
            })?,
            tip,
            current,
            &bytes,
        )?;
        Ok(())
    }

    /// The mode `git add` would record for a path the tip does not hold.
    pub fn mode_for_new(&self, path: &RepoPath, file_mode_honoured: bool) -> u32 {
        self.done
            .iter()
            .find(|done| done.path == *path)
            .filter(|done| file_mode_honoured && done.executable)
            .map_or(0o100644, |_| 0o100755)
    }

    /// Undo every placement, in reverse order.
    ///
    /// What a rollback takes away it takes by exchange or rename, never by
    /// unlink: the entry may be an editor's inode by then, and an unlink would
    /// take its only name.
    pub fn undo_all(&mut self) -> Vec<Undone> {
        let mut outcomes = Vec::new();
        for index in (0..self.done.len()).rev() {
            outcomes.push(self.undo_one(index));
        }
        outcomes
    }

    /// Undo the placements the caller selects, in reverse order. Step 5 keeps
    /// a path where the deciding tip holds exactly what step 3 recorded.
    pub fn undo_selected(&mut self, keep: &dyn Fn(&str) -> bool) -> Vec<Undone> {
        let mut outcomes = Vec::new();
        for index in (0..self.done.len()).rev() {
            if !keep(&self.done[index].path.to_string()) {
                outcomes.push(self.undo_one(index));
            }
        }
        outcomes
    }

    fn undo_one(&mut self, index: usize) -> Undone {
        let done = &mut self.done[index];
        let path = done.path.to_string();
        if done.undone {
            return Undone::Restored { path };
        }
        if done.created {
            let away = OsString::from(format!("{}.rolled-back", done.temporary.to_string_lossy()));
            match done.parent.rename(&done.leaf, &away) {
                Ok(()) => {
                    done.undone = true;
                    Undone::RenamedAway {
                        path,
                        to: away.to_string_lossy().into_owned(),
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    done.undone = true;
                    Undone::RenamedAway {
                        path,
                        to: away.to_string_lossy().into_owned(),
                    }
                }
                Err(e) => Undone::Reported {
                    path,
                    detail: e.to_string(),
                },
            }
        } else {
            match done.parent.exchange(&done.leaf, &done.temporary) {
                Ok(()) => {
                    done.undone = true;
                    Undone::Restored { path }
                }
                Err(e) => Undone::Reported {
                    path,
                    detail: e.to_string(),
                },
            }
        }
    }

    /// After step 5 has moved the branch: the `link()` source is the one name
    /// the UI ever unlinks, so that the new file has one name, at the path,
    /// and `git status` shows nothing untracked.
    pub fn finish(&mut self, previous_directory: &Dir) -> Vec<Undone> {
        let mut reported = Vec::new();
        for done in &mut self.done {
            if done.undone {
                continue;
            }
            if done.created {
                if let Err(e) = done.parent.unlink(&done.temporary)
                    && e.kind() != std::io::ErrorKind::NotFound
                {
                    reported.push(Undone::Reported {
                        path: done.path.to_string(),
                        detail: e.to_string(),
                    });
                }
            } else if let Err(detail) = retain(&done.parent, &done.temporary, previous_directory) {
                // Where the retention directory is on another filesystem and
                // the move fails, the file stays beside the path under its
                // temporary name and the page says so.
                reported.push(Undone::Reported {
                    path: done.path.to_string(),
                    detail,
                });
            }
        }
        reported
    }

    /// Take the replacements out of the working tree after an undo.
    ///
    /// An exchange that is swapped back leaves the *replacement* under the
    /// temporary name beside the path, and that file was at the path for a
    /// moment: an editor that opened it there holds its inode and may save
    /// through it, so it is moved under `previous/` and kept, never deleted —
    /// exactly as a swapped-out original is. A `link()` that was renamed away
    /// is moved the same way, and its own source unlinked, that being the one
    /// name this UI ever unlinks.
    pub fn retain_undone(&mut self, previous: &Dir, id: &str) -> Vec<Undone> {
        let mut reported = Vec::new();
        for done in &mut self.done {
            if !done.undone {
                continue;
            }
            let names = if done.created {
                vec![
                    OsString::from(format!("{}.rolled-back", done.temporary.to_string_lossy())),
                    done.temporary.clone(),
                ]
            } else {
                vec![done.temporary.clone()]
            };
            for name in names {
                match move_beside(&done.parent, &name, previous, id) {
                    Ok(()) => {}
                    Err(detail) if detail == "absent" => {}
                    Err(detail) => reported.push(Undone::Reported {
                        path: done.path.to_string(),
                        detail,
                    }),
                }
            }
        }
        reported
    }

    /// The names the page shows for what a refused compose left beside a path.
    pub fn retained_names(&self) -> Vec<String> {
        self.done
            .iter()
            .map(|done| done.temporary.to_string_lossy().into_owned())
            .collect()
    }
}

/// A replacement written and checked but not yet at the path.
#[derive(Debug)]
pub struct Prepared {
    path: RepoPath,
    parent: Dir,
    leaf: OsString,
    temporary: OsString,
    handle: FileAt,
    previous: Option<Previous>,
    executable: bool,
    bytes: Vec<u8>,
}

impl Prepared {
    pub fn path(&self) -> &RepoPath {
        &self.path
    }

    /// Drop a prepared replacement that will never be installed, which is the
    /// one removal that needs no care: the name is this process's own, made
    /// with `O_EXCL` a moment ago.
    pub fn discard(self) {
        let _ = self.parent.unlink(&self.temporary);
    }
}

/// Move a swapped-out file under `<git-dir>/pbps-ui/previous/`.
///
/// Never deleted: an editor that opened the file before the exchange holds a
/// descriptor to that inode and may write through it after the hash, and an
/// unlinked inode would take that save with it. A late write then lands in a
/// file the user can find, not in one that no longer has a name.
fn retain(parent: &Dir, temporary: &OsString, previous: &Dir) -> Result<(), String> {
    let bytes = parent
        .open_file(temporary)
        .and_then(|file| file.read())
        .map_err(|e| e.to_string())?;
    let name = OsString::from(format!(
        "{}-{}",
        temporary.to_string_lossy(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|since| since.as_secs())
            .unwrap_or_default()
    ));
    let kept = previous
        .create_new(&name, 0o600)
        .map_err(|e| e.to_string())?;
    kept.write_all(&bytes).map_err(|e| e.to_string())?;
    kept.flush().map_err(|e| e.to_string())?;
    parent.unlink(temporary).map_err(|e| e.to_string())
}

/// Move one name out of the working tree and under the retention directory.
///
/// `Err("absent")` where the name is already gone, which is what running this
/// twice looks like and is not a failure.
fn move_beside(parent: &Dir, name: &OsString, previous: &Dir, id: &str) -> Result<(), String> {
    let file = match parent.open_file(name) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err("absent".to_owned()),
        Err(e) => return Err(e.to_string()),
    };
    let bytes = file.read().map_err(|e| e.to_string())?;
    let kept_as = OsString::from(format!("{}-{id}", name.to_string_lossy()));
    let kept = match previous.create_new(&kept_as, 0o600) {
        Ok(kept) => kept,
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => return Ok(()),
        Err(e) => return Err(e.to_string()),
    };
    kept.write_all(&bytes).map_err(|e| e.to_string())?;
    kept.flush().map_err(|e| e.to_string())?;
    parent.unlink(name).map_err(|e| e.to_string())
}

/// Turn a leaf name into the one the temporary uses.
pub fn temporary_name(path: &RepoPath, suffix: &str) -> String {
    let leaf = path.components().last().unwrap_or(b"file");
    format!(
        "{}.pbps-ui-{suffix}",
        String::from_utf8_lossy(std::ffi::OsStr::from_bytes(leaf).as_bytes())
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compose::scratch_repo::Scratch;

    fn path(text: &str) -> RepoPath {
        RepoPath::new(text.as_bytes()).unwrap()
    }

    fn wanted(scratch: &Scratch, git: &Git, relative: &str, bytes: &[u8]) -> Wanted {
        let file = scratch.path(relative);
        Wanted {
            path: path(relative),
            bytes: bytes.to_vec(),
            shown: Some(
                std::fs::read(&file)
                    .ok()
                    .map(|old| attributes::hash(git, &old).unwrap()),
            ),
        }
    }

    fn place_one(
        placement: &mut Placement,
        git: &Git,
        tip: &str,
        wanted: &Wanted,
    ) -> Result<(), PlaceRefusal> {
        let (_, prepared) = placement.intend(git, wanted, "test")?;
        placement.install(git, tip, prepared)
    }

    #[test]
    fn an_exchange_puts_the_replacement_at_the_path_and_keeps_what_came_out() {
        let scratch = Scratch::new("place-exchange");
        scratch.write("schema/a.yml", b"table: before\n");
        let tip = scratch.commit("one");
        let git = scratch.runner();
        let mut placement = Placement::new(Dir::open_root(&scratch.root).unwrap());

        let wanted = wanted(&scratch, &git, "schema/a.yml", b"table: after\n");
        place_one(&mut placement, &git, &tip, &wanted).unwrap();

        assert_eq!(
            std::fs::read(scratch.path("schema/a.yml")).unwrap(),
            b"table: after\n"
        );
        assert_eq!(
            std::fs::read(scratch.path("schema/a.yml.pbps-ui-test")).unwrap(),
            b"table: before\n",
            "what came out is still a file, under a name"
        );

        placement.undo_all();
        assert_eq!(
            std::fs::read(scratch.path("schema/a.yml")).unwrap(),
            b"table: before\n",
            "and the undo puts it back"
        );
    }

    #[test]
    fn a_refusal_on_the_second_path_leaves_the_first_one_as_it_was() {
        // An intent may edit more than one file, and a refusal that left some
        // of them replaced would be a partial edit nobody asked for.
        let scratch = Scratch::new("place-partial");
        scratch.write("schema/a.yml", b"table: a\n");
        scratch.write("schema/b.yml", b"table: b\n");
        let tip = scratch.commit("one");
        let git = scratch.runner();
        let mut placement = Placement::new(Dir::open_root(&scratch.root).unwrap());

        let first = wanted(&scratch, &git, "schema/a.yml", b"table: a2\n");
        place_one(&mut placement, &git, &tip, &first).unwrap();

        // The second path is a link, which this UI never composes through.
        std::fs::remove_file(scratch.path("schema/b.yml")).unwrap();
        std::os::unix::fs::symlink("a.yml", scratch.path("schema/b.yml")).unwrap();
        let second = Wanted {
            path: path("schema/b.yml"),
            bytes: b"table: b2\n".to_vec(),
            shown: None,
        };
        let refusal = place_one(&mut placement, &git, &tip, &second).unwrap_err();
        assert!(
            matches!(refusal, PlaceRefusal::NotARegularFile { .. }),
            "got {refusal}"
        );

        placement.undo_all();
        assert_eq!(
            std::fs::read(scratch.path("schema/a.yml")).unwrap(),
            b"table: a\n",
            "the first path is back where it started"
        );
    }

    #[test]
    fn an_editor_saving_between_the_read_and_the_placement_is_refused() {
        // The locks stop `git`, not an editor saving the same file, and the
        // browser's copy would otherwise overwrite newer work.
        let scratch = Scratch::new("place-raced");
        scratch.write("schema/a.yml", b"table: as read\n");
        let tip = scratch.commit("one");
        let git = scratch.runner();
        let mut placement = Placement::new(Dir::open_root(&scratch.root).unwrap());
        let wanted = wanted(&scratch, &git, "schema/a.yml", b"table: from the page\n");

        scratch.write("schema/a.yml", b"table: the editor's newer work\n");

        let refusal = place_one(&mut placement, &git, &tip, &wanted).unwrap_err();
        assert!(
            matches!(refusal, PlaceRefusal::Changed { .. }),
            "got {refusal}"
        );
        assert_eq!(
            std::fs::read(scratch.path("schema/a.yml")).unwrap(),
            b"table: the editor's newer work\n",
            "and the newer work is untouched"
        );
    }

    #[test]
    fn a_new_declaration_is_linked_and_its_undo_never_unlinks_an_editors_inode() {
        let scratch = Scratch::new("place-new");
        scratch.write("schema/a.yml", b"table: a\n");
        let tip = scratch.commit("one");
        let git = scratch.runner();
        let mut placement = Placement::new(Dir::open_root(&scratch.root).unwrap());

        let wanted = Wanted {
            path: path("schema/new.yml"),
            bytes: b"table: new\n".to_vec(),
            shown: None,
        };
        place_one(&mut placement, &git, &tip, &wanted).unwrap();
        assert_eq!(
            std::fs::read(scratch.path("schema/new.yml")).unwrap(),
            b"table: new\n"
        );

        // An editor saves by writing a temporary and renaming it over, so the
        // path now holds *its* inode with a link count of one.
        scratch.write("schema/.swap", b"the editor's own\n");
        std::fs::rename(scratch.path("schema/.swap"), scratch.path("schema/new.yml")).unwrap();

        let outcomes = placement.undo_all();
        assert!(
            matches!(outcomes.as_slice(), [Undone::RenamedAway { .. }]),
            "{outcomes:?}"
        );
        assert!(!scratch.path("schema/new.yml").exists());
        let kept: Vec<_> = std::fs::read_dir(scratch.path("schema"))
            .unwrap()
            .filter_map(Result::ok)
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains("rolled-back"))
            .collect();
        assert_eq!(kept.len(), 1, "{kept:?}");
        assert_eq!(
            std::fs::read(scratch.path("schema").join(&kept[0])).unwrap(),
            b"the editor's own\n",
            "the editor's bytes are under a name they can find"
        );
    }

    #[test]
    fn finishing_keeps_the_swapped_out_file_and_unlinks_only_the_links_own_source() {
        let scratch = Scratch::new("place-finish");
        scratch.write("schema/a.yml", b"table: before\n");
        let tip = scratch.commit("one");
        let git = scratch.runner();
        let previous_directory = scratch.path(".git/pbps-ui/previous");
        std::fs::create_dir_all(&previous_directory).unwrap();
        let previous = Dir::open_root(&previous_directory).unwrap();
        let mut placement = Placement::new(Dir::open_root(&scratch.root).unwrap());

        let edited = wanted(&scratch, &git, "schema/a.yml", b"table: after\n");
        place_one(&mut placement, &git, &tip, &edited).unwrap();
        let added = Wanted {
            path: path("schema/new.yml"),
            bytes: b"table: new\n".to_vec(),
            shown: None,
        };
        place_one(&mut placement, &git, &tip, &added).unwrap();

        let reported = placement.finish(&previous);
        assert!(reported.is_empty(), "{reported:?}");

        assert!(
            !scratch.path("schema/a.yml.pbps-ui-test").exists(),
            "the swapped-out file has moved out of the working tree"
        );
        assert!(
            !scratch.path("schema/new.yml.pbps-ui-test").exists(),
            "and the link's own source is the one name that is unlinked"
        );
        let kept: Vec<_> = std::fs::read_dir(&previous_directory)
            .unwrap()
            .filter_map(Result::ok)
            .map(|e| std::fs::read(e.path()).unwrap())
            .collect();
        assert_eq!(kept, vec![b"table: before\n".to_vec()]);
        assert_eq!(
            std::fs::read(scratch.path("schema/new.yml")).unwrap(),
            b"table: new\n"
        );
    }

    #[test]
    fn a_name_something_else_created_in_the_window_is_reported_not_replaced() {
        let scratch = Scratch::new("place-appeared");
        scratch.write("schema/a.yml", b"table: a\n");
        let tip = scratch.commit("one");
        let git = scratch.runner();
        let mut placement = Placement::new(Dir::open_root(&scratch.root).unwrap());
        let wanted = Wanted {
            path: path("schema/new.yml"),
            bytes: b"table: new\n".to_vec(),
            shown: None,
        };

        let (_, prepared) = placement.intend(&git, &wanted, "test").unwrap();
        scratch.write("schema/new.yml", b"somebody else got there first\n");

        let refusal = placement.install(&git, &tip, prepared).unwrap_err();
        assert!(
            matches!(refusal, PlaceRefusal::Appeared { .. }),
            "got {refusal}"
        );
        assert_eq!(
            std::fs::read(scratch.path("schema/new.yml")).unwrap(),
            b"somebody else got there first\n"
        );
    }

    #[test]
    fn a_path_the_preview_expected_to_be_absent_is_refused_when_something_made_it() {
        // Two options and not one. Collapsing "the preview expected nothing
        // here" into "the preview said nothing" lets a file created after the
        // last check be accepted as an existing one and exchanged away, and
        // the commit then holds bytes that replaced a creation nobody
        // previewed.
        let scratch = Scratch::new("place-expected-absent");
        scratch.write("schema/a.yml", b"table: a\n");
        let tip = scratch.commit("one");
        let git = scratch.runner();
        let mut placement = Placement::new(Dir::open_root(&scratch.root).unwrap());

        // Somebody created it between the preview and now.
        scratch.write("schema/new.yml", b"somebody else's work\n");

        let expecting_absence = Wanted {
            path: path("schema/new.yml"),
            bytes: b"table: new\n".to_vec(),
            shown: Some(None),
        };
        let refusal = place_one(&mut placement, &git, &tip, &expecting_absence).unwrap_err();
        assert!(
            matches!(refusal, PlaceRefusal::Appeared { .. }),
            "got {refusal}"
        );
        assert_eq!(
            std::fs::read(scratch.path("schema/new.yml")).unwrap(),
            b"somebody else's work\n"
        );

        // And the control: with no expectation at all the same placement is
        // an ordinary exchange, which is why the two cases cannot share a
        // representation.
        let no_expectation = Wanted {
            path: path("schema/new.yml"),
            bytes: b"table: new\n".to_vec(),
            shown: None,
        };
        place_one(&mut placement, &git, &tip, &no_expectation).unwrap();
        assert_eq!(
            std::fs::read(scratch.path("schema/new.yml")).unwrap(),
            b"table: new\n"
        );
    }

    #[test]
    fn a_path_the_preview_expected_to_hold_something_is_refused_when_it_is_gone() {
        let scratch = Scratch::new("place-expected-content");
        scratch.write("schema/a.yml", b"table: a\n");
        let tip = scratch.commit("one");
        let git = scratch.runner();
        let mut placement = Placement::new(Dir::open_root(&scratch.root).unwrap());
        let blob = attributes::hash(&git, b"table: a\n").unwrap();
        std::fs::remove_file(scratch.path("schema/a.yml")).unwrap();

        let refusal = place_one(
            &mut placement,
            &git,
            &tip,
            &Wanted {
                path: path("schema/a.yml"),
                bytes: b"table: a2\n".to_vec(),
                shown: Some(Some(blob)),
            },
        )
        .unwrap_err();
        assert!(
            matches!(refusal, PlaceRefusal::Changed { .. }),
            "got {refusal}"
        );
    }

    #[test]
    fn the_record_names_both_sides_of_the_exchange_before_it_happens() {
        // With only the name, a recovery would find a file at the path and a
        // file beside it and no way to tell which is which.
        let scratch = Scratch::new("place-intent");
        scratch.write("schema/a.yml", b"table: before\n");
        let tip = scratch.commit("one");
        let git = scratch.runner();
        let placement = Placement::new(Dir::open_root(&scratch.root).unwrap());
        let wanted = wanted(&scratch, &git, "schema/a.yml", b"table: after\n");

        let (intent, prepared) = placement.intend(&git, &wanted, "test").unwrap();

        assert_eq!(intent.path, "schema/a.yml");
        assert_eq!(intent.temporary, "a.yml.pbps-ui-test");
        let (original_blob, _, original_inode) =
            intent.original.clone().expect("the path had a file");
        assert_eq!(Some(original_blob), wanted.shown.clone().unwrap());
        assert_ne!(
            original_inode, intent.replacement.2,
            "the two identities are what tell the sides apart"
        );
        assert_eq!(
            intent.replacement.0,
            attributes::hash(&git, b"table: after\n").unwrap()
        );
        // Nothing is at the path yet: everything so far is reversible by
        // removing one temporary file.
        assert_eq!(
            std::fs::read(scratch.path("schema/a.yml")).unwrap(),
            b"table: before\n"
        );
        prepared.discard();
        assert!(!scratch.path("schema/a.yml.pbps-ui-test").exists());
        let _ = tip;
    }
}
