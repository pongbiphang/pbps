//! Every source read is relative to a directory handle and refuses symlinks.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::Read;
use std::os::fd::{AsFd, OwnedFd};
use std::path::{Component, Path};

use rustix::fs::{AtFlags, FileType, Mode, OFlags, ResolveFlags, fstat, openat2, statat};
use serde::Serialize;

use super::{Error, Result, digest};

/// `statfs` type of a 9p mount: how WSL2 serves a Windows drive (`/mnt/c`,
/// `aname=drvfs`), measured on WSL2 6.6 (#1070).
const V9FS_MAGIC: u64 = 0x0102_1997;

/// Why compose's guarantees are not known to hold on a filesystem, or `None`.
/// ADR-0017 rests on POSIX directory fsync, `flock`, rename over open files
/// and stable inode identity; on a 9p mount these reach NTFS through a
/// translation layer that has not been qualified. Everything else is left
/// to the ordinary checks, as before.
fn unqualified_filesystem(kind: u64) -> Option<&'static str> {
    (kind == V9FS_MAGIC).then_some(
        "is on a 9p mount, such as a Windows drive under WSL (/mnt/c), where compose's \
         durability and locking guarantees are not qualified; move the checkout into the \
         WSL filesystem (for example ~/project) and run pbps ui there",
    )
}

/// Refuses a compose root or store whose filesystem is not qualified,
/// decided from the handle already open, never from the path's spelling.
pub(super) fn qualified_filesystem(handle: impl AsFd, what: &str) -> Result<()> {
    let stat = rustix::fs::fstatfs(handle)
        .map_err(|_| Error::new(&format!("Could not inspect the filesystem of {what}")))?;
    // `f_type` is a signed word on some targets; the magic numbers are
    // non-negative 32-bit values, so widening through `i64` is exact.
    #[allow(clippy::unnecessary_cast)]
    let kind = stat.f_type as i64 as u64;
    match unqualified_filesystem(kind) {
        Some(reason) => Err(Error::new(&format!("The {what} {reason}"))),
        None => Ok(()),
    }
}

pub(super) const MAX_BYTES: usize = 64 * 1024 * 1024;
pub(super) const MAX_FILES: usize = 4096;

pub(super) fn declaration_path(name: &str) -> bool {
    // Match the CLI loader's case-sensitive suffix vocabulary. Other files
    // beneath schema_dir are not declarations and must stay outside the diff.
    matches!(
        Path::new(name).extension().and_then(|s| s.to_str()),
        Some("yml" | "yaml")
    )
}

pub(super) fn path(name: &str) -> Result<&str> {
    if name.is_empty()
        || name.contains(['\0', '\\', '\n', '\r'])
        || name.split('/').any(|part| {
            part.is_empty() || part == "." || part == ".." || part.eq_ignore_ascii_case(".git")
        })
        || Path::new(name)
            .components()
            .any(|c| !matches!(c, Component::Normal(_)))
    {
        return Err(Error::new("Compose requires contained regular-file paths"));
    }
    Ok(name)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct FileBytes {
    pub bytes: Vec<u8>,
    pub mode: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Evidence {
    pub digest: String,
    pub mode: u32,
}

impl FileBytes {
    pub fn evidence(&self) -> Evidence {
        Evidence {
            digest: digest(&self.bytes),
            mode: self.mode,
        }
    }
}

pub(super) struct Root(OwnedFd);

impl Root {
    pub fn same_directory(&self, other: &Self) -> Result<bool> {
        let a = fstat(&self.0).map_err(|_| Error::new("Could not inspect a compose root"))?;
        let b = fstat(&other.0).map_err(|_| Error::new("Could not inspect a compose root"))?;
        Ok(a.st_dev == b.st_dev && a.st_ino == b.st_ino)
    }
    pub fn open(root: &Path) -> Result<Self> {
        let fd = openat2(
            rustix::fs::CWD,
            root,
            OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
            ResolveFlags::NO_SYMLINKS,
        )
        .map_err(|_| Error::new("Could not open a no-follow compose root"))?;
        qualified_filesystem(&fd, "project checkout")?;
        Ok(Self(fd))
    }

    pub fn read(&self, name: &str) -> Result<Option<FileBytes>> {
        self.read_file(name, false)
    }

    pub fn input(&self, name: &str, declarations: &str) -> Result<Option<FileBytes>> {
        self.read_file(
            name,
            name.starts_with(&format!("{declarations}/")) && declaration_path(name),
        )
    }

    fn read_file(&self, name: &str, directory_is_absent: bool) -> Result<Option<FileBytes>> {
        path(name)?;
        let fd = match openat2(
            &self.0,
            name,
            OFlags::RDONLY | OFlags::NONBLOCK | OFlags::CLOEXEC,
            Mode::empty(),
            ResolveFlags::BENEATH | ResolveFlags::NO_SYMLINKS | ResolveFlags::NO_XDEV,
        ) {
            Ok(fd) => fd,
            // A former descendant also becomes absent when an ancestor is
            // replaced by a file. NO_SYMLINKS still refuses linked parents;
            // declaration enumeration separately refuses every special entry.
            Err(rustix::io::Errno::NOENT | rustix::io::Errno::NOTDIR) => return Ok(None),
            Err(_) => {
                return Err(Error::new(
                    "A compose input is unreadable or not a contained regular file",
                ));
            }
        };
        // An input can sit on a mount beneath a qualified root (DEC-1070.1).
        qualified_filesystem(&fd, "compose input")?;
        let before = fstat(&fd).map_err(|_| Error::new("Could not inspect a compose input"))?;
        // The recursive loader treats a directory ending in .yml as a
        // directory. It has no declaration blob at its own pathname.
        if directory_is_absent && FileType::from_raw_mode(before.st_mode) == FileType::Directory {
            return Ok(None);
        }
        if FileType::from_raw_mode(before.st_mode) != FileType::RegularFile {
            return Err(Error::new("Compose inputs must be regular files"));
        }
        let mut file = File::from(fd);
        let mut bytes = Vec::new();
        file.by_ref()
            .take(MAX_BYTES as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| Error::new("Could not read a compose input"))?;
        let after = fstat(&file).map_err(|_| Error::new("Could not recheck a compose input"))?;
        if bytes.len() > MAX_BYTES {
            return Err(Error::rejected("A compose input exceeds the size limit"));
        }
        if before.st_size != after.st_size
            || before.st_mtime != after.st_mtime
            || before.st_mtime_nsec != after.st_mtime_nsec
            || before.st_ctime != after.st_ctime
            || before.st_ctime_nsec != after.st_ctime_nsec
            || before.st_mode != after.st_mode
        {
            return Err(Error::new(
                "A compose input changed while being captured or exceeds the size limit",
            ));
        }
        Ok(Some(FileBytes {
            bytes,
            // Git's executable mode follows the owner's execute bit; group
            // or other execute permissions alone do not make a 100755 entry.
            mode: if before.st_mode & 0o100 == 0 {
                0o100644
            } else {
                0o100755
            },
        }))
    }

    pub fn declarations(&self, directory: &str) -> Result<BTreeMap<String, FileBytes>> {
        path(directory)?;
        let fd = openat2(
            &self.0,
            directory,
            OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
            ResolveFlags::BENEATH | ResolveFlags::NO_SYMLINKS | ResolveFlags::NO_XDEV,
        )
        .map_err(|_| Error::new("Could not enumerate the contained declarations directory"))?;
        let mut names = Vec::new();
        Self::walk(&fd, directory, &mut names, 0, &mut 0)?;
        let mut result = BTreeMap::new();
        let mut bytes = 0;
        for name in names {
            let Some(contents) = self.read(&name)? else {
                return Err(Error::new("Declaration membership changed during capture"));
            };
            bytes += contents.bytes.len();
            if bytes > MAX_BYTES {
                return Err(Error::rejected(
                    "Compose inputs exceed the total size limit",
                ));
            }
            result.insert(name, contents);
        }
        Ok(result)
    }

    fn walk(
        fd: &OwnedFd,
        prefix: &str,
        names: &mut Vec<String>,
        depth: usize,
        count: &mut usize,
    ) -> Result<()> {
        if depth > 64 {
            return Err(Error::rejected(
                "Compose declarations exceed the directory depth limit",
            ));
        }
        let mut directory = rustix::fs::Dir::read_from(fd)
            .map_err(|_| Error::new("Could not enumerate compose declarations"))?;
        while let Some(entry) = directory.read() {
            let entry = entry
                .map_err(|_| Error::new("Could not finish enumerating compose declarations"))?;
            let name = entry
                .file_name()
                .to_str()
                .map_err(|_| Error::new("Compose paths must be UTF-8"))?;
            if name == "." || name == ".." {
                continue;
            }
            *count += 1;
            if *count > MAX_FILES {
                return Err(Error::rejected("Compose has too many input paths"));
            }
            let full = format!("{prefix}/{name}");
            path(&full)?;
            let stat = statat(fd, name, AtFlags::SYMLINK_NOFOLLOW)
                .map_err(|_| Error::new("Declaration membership changed during capture"))?;
            match FileType::from_raw_mode(stat.st_mode) {
                FileType::Directory => {
                    let child = openat2(
                        fd,
                        name,
                        OFlags::DIRECTORY | OFlags::CLOEXEC,
                        Mode::empty(),
                        ResolveFlags::BENEATH | ResolveFlags::NO_SYMLINKS | ResolveFlags::NO_XDEV,
                    )
                    .map_err(|_| Error::new("A declaration directory changed during capture"))?;
                    Self::walk(&child, &full, names, depth + 1, count)?;
                }
                FileType::RegularFile => {
                    if declaration_path(name) {
                        names.push(full);
                    }
                }
                FileType::Symlink
                | FileType::Fifo
                | FileType::Socket
                | FileType::CharacterDevice
                | FileType::BlockDevice
                | FileType::Unknown => {
                    return Err(Error::new(
                        "Compose declaration trees cannot contain links or special files",
                    ));
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod filesystem_tests {
    use super::*;

    #[test]
    fn a_windows_drive_under_wsl_is_refused_and_ordinary_filesystems_are_not() {
        // 9p, as WSL2 serves /mnt/c: measured on WSL2 6.6 (#1070).
        let reason = unqualified_filesystem(0x0102_1997).unwrap();
        assert!(reason.contains("WSL filesystem"), "{reason}");
        for (name, kind) in [
            ("ext4", 0xEF53),
            ("tmpfs", 0x0102_1994),
            ("xfs", 0x5846_5342),
            ("btrfs", 0x9123_683E),
            ("overlayfs", 0x794C_7630),
        ] {
            assert_eq!(unqualified_filesystem(kind), None, "{name}");
        }
    }

    #[test]
    fn an_ordinary_local_directory_is_qualified() {
        let root = Root::open(&std::env::temp_dir()).unwrap();
        assert!(qualified_filesystem(&root.0, "test directory").is_ok());
    }
}
