//! An acknowledged private tree can be retired without sweeping unknown files.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path};

use rustix::fs::{AtFlags, Mode, OFlags, ResolveFlags, openat2};
use serde::{Deserialize, Serialize};

use super::{
    Error, Result,
    durable::{Directory, Identity, ResourceOperation},
};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Entry {
    identity: Identity,
    directory: bool,
    size: u64,
    modified: (i64, i64),
    changed: (i64, i64),
}

impl Entry {
    fn of(file: &File) -> Result<Self> {
        let m = file
            .metadata()
            .map_err(|_| Error::new("Cannot inspect private snapshot ownership"))?;
        if (!m.is_dir() && !m.is_file())
            || m.uid() != rustix::process::geteuid().as_raw()
            || (m.is_file() && m.nlink() != 1)
        {
            return Err(Error::new(
                "Private snapshot contains unowned or special evidence",
            ));
        }
        Ok(Self {
            identity: Identity::of(file)?,
            directory: m.is_dir(),
            size: m.len(),
            modified: (m.mtime(), m.mtime_nsec()),
            changed: (m.ctime(), m.ctime_nsec()),
        })
    }

    fn matches(&self, other: &Self) -> bool {
        // Removing children changes a directory's timestamps and size.
        self.identity == other.identity
            && self.directory == other.directory
            && (self.directory || self == other)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Inventory {
    pub root: Identity,
    entries: BTreeMap<String, Entry>,
}

fn relative(path: &str) -> Result<()> {
    if path.is_empty()
        || path.contains(['\0', '\\'])
        || Path::new(path)
            .components()
            .any(|p| !matches!(p, Component::Normal(_)))
    {
        return Err(Error::new("Invalid private snapshot inventory path"));
    }
    Ok(())
}

fn open(root: &File, path: &str) -> Result<Option<File>> {
    relative(path)?;
    match openat2(
        root,
        path,
        OFlags::RDONLY | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
        ResolveFlags::BENEATH | ResolveFlags::NO_SYMLINKS | ResolveFlags::NO_XDEV,
    ) {
        Ok(fd) => Ok(Some(File::from(fd))),
        Err(rustix::io::Errno::NOENT) => Ok(None),
        Err(_) => Err(Error::new(
            "Private snapshot evidence is unreadable or conflicting",
        )),
    }
}

impl Inventory {
    pub fn before_check(root: &Directory, known: &BTreeSet<String>) -> Result<Self> {
        let inventory = Self::capture(root)?;
        // Only paths from completed fixed preparation steps can enter the
        // rejection inventory. In particular, a leftover lock or unknown
        // temporary file is not adopted merely because it is in our directory.
        if inventory.entries.keys().any(|name| !known.contains(name)) {
            return Err(Error::new(
                "Unknown private snapshot entries require manual recovery",
            ));
        }
        Ok(inventory)
    }

    pub fn capture(root: &Directory) -> Result<Self> {
        let mut entries = BTreeMap::new();
        fn walk(
            root: &Directory,
            directory: &File,
            prefix: &str,
            entries: &mut BTreeMap<String, Entry>,
            depth: usize,
        ) -> Result<()> {
            if depth > 80 || entries.len() > 32768 {
                return Err(Error::new("Private snapshot inventory exceeds its bound"));
            }
            let path = root.path.join(prefix);
            root.observer.at(ResourceOperation::Read, false, &path)?;
            let mut stream = rustix::fs::Dir::read_from(directory)
                .map_err(|_| Error::new("Cannot enumerate private snapshot ownership"))?;
            while let Some(entry) = stream.read() {
                let entry =
                    entry.map_err(|_| Error::new("Private snapshot enumeration is incomplete"))?;
                let name = entry
                    .file_name()
                    .to_str()
                    .map_err(|_| Error::new("Private snapshot name is unsupported"))?;
                if name == "." || name == ".." {
                    continue;
                }
                let relative = if prefix.is_empty() {
                    name.to_owned()
                } else {
                    format!("{prefix}/{name}")
                };
                let file = open(&root.file, &relative)?
                    .ok_or_else(|| Error::new("Private snapshot changed during sealing"))?;
                let evidence = Entry::of(&file)?;
                if evidence.directory {
                    walk(root, &file, &relative, entries, depth + 1)?;
                }
                root.observer
                    .at(ResourceOperation::Sync, false, &root.path.join(&relative))?;
                file.sync_all()
                    .map_err(|_| Error::new("Private snapshot flush is unresolved"))?;
                root.observer
                    .at(ResourceOperation::Sync, true, &root.path.join(&relative))?;
                if evidence != Entry::of(&file)? {
                    return Err(Error::new("Private snapshot changed while flushing"));
                }
                if entries.len() >= 32768 {
                    return Err(Error::new("Private snapshot inventory exceeds its bound"));
                }
                entries.insert(relative, evidence);
            }
            root.observer.at(ResourceOperation::Read, true, &path)?;
            Ok(())
        }
        root.check()?;
        walk(root, &root.file, "", &mut entries, 0)?;
        root.sync()?;
        Ok(Self {
            root: root.identity()?,
            entries,
        })
    }

    pub fn retire(&self, parent: &Directory, id: &str) -> Result<()> {
        // The caller has durably entered retirement. Absence now discharges an
        // acknowledged resource; an unrecorded name can never use this path.
        let Some(root_file) = open(&parent.file, id)? else {
            return parent.sync();
        };
        if Identity::of(&root_file)? != self.root {
            return Err(Error::new("Private snapshot was replaced; preserve it"));
        }
        let root = Directory {
            path: parent.path.join(id),
            file: root_file,
            observer: parent.observer.clone(),
        };
        root.private()?;
        let mut entries: Vec<_> = self.entries.iter().collect();
        entries.sort_by_key(|(name, _)| std::cmp::Reverse((name.matches('/').count(), *name)));
        for (name, expected) in entries {
            relative(name)?;
            let path = root.path.join(name);
            root.observer.at(ResourceOperation::Remove, false, &path)?;
            root.check()?;
            let (parent_name, leaf) = name.rsplit_once('/').unwrap_or(("", name));
            let directory = if parent_name.is_empty() {
                root.file
                    .try_clone()
                    .map_err(|_| Error::new("Cannot retain private snapshot anchor"))?
            } else {
                let Some(file) = open(&root.file, parent_name)? else {
                    continue;
                };
                let expected_parent = self
                    .entries
                    .get(parent_name)
                    .ok_or_else(|| Error::new("Incomplete snapshot parent ownership"))?;
                if !expected_parent.directory || !expected_parent.matches(&Entry::of(&file)?) {
                    return Err(Error::new(
                        "Private snapshot parent was replaced; preserve it",
                    ));
                }
                file
            };
            if let Some(file) = open(&directory, leaf)? {
                if !expected.matches(&Entry::of(&file)?) {
                    return Err(Error::new("Private snapshot resource changed; preserve it"));
                }
                rustix::fs::unlinkat(
                    &directory,
                    leaf,
                    if expected.directory {
                        AtFlags::REMOVEDIR
                    } else {
                        AtFlags::empty()
                    },
                )
                .map_err(|_| {
                    Error::new("Private snapshot retirement is incomplete; preserve its inventory")
                })?;
            }
            root.observer.at(ResourceOperation::Remove, true, &path)?;
            root.observer
                .at(ResourceOperation::Sync, false, &root.path.join(parent_name))?;
            directory
                .sync_all()
                .map_err(|_| Error::new("Private snapshot retirement flush is unresolved"))?;
            root.observer
                .at(ResourceOperation::Sync, true, &root.path.join(parent_name))?;
        }
        parent
            .observer
            .at(ResourceOperation::Remove, false, &root.path)?;
        root.check()?;
        parent.check()?;
        rustix::fs::unlinkat(&parent.file, id, AtFlags::REMOVEDIR).map_err(|_| {
            Error::new("Private snapshot contains unresolved entries; preserve its inventory")
        })?;
        parent
            .observer
            .at(ResourceOperation::Remove, true, &root.path)?;
        parent.sync()
    }
}
