//! Descriptor-relative persistence; a pathname is never a cleanup capability.

use std::fs::File;
use std::io::{Read, Write};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use rustix::fs::{AtFlags, Mode, OFlags, ResolveFlags, openat2};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::{Error, Result, random_id};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResourceOperation {
    Identify,
    Open,
    Create,
    Read,
    Write,
    Sync,
    Rename,
    Remove,
    Unlock,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResourceBoundary {
    pub operation: ResourceOperation,
    pub after: bool,
    pub path: PathBuf,
}

/// In-process scheduling only; neither HTTP nor persisted data selects a hook.
#[derive(Clone)]
pub struct ResourceObserver(Arc<dyn Fn(&ResourceBoundary) -> bool + Send + Sync>);

impl Default for ResourceObserver {
    fn default() -> Self {
        Self(Arc::new(|_| true))
    }
}

impl ResourceObserver {
    pub fn new(observer: impl Fn(&ResourceBoundary) -> bool + Send + Sync + 'static) -> Self {
        Self(Arc::new(observer))
    }

    pub(super) fn at(&self, operation: ResourceOperation, after: bool, path: &Path) -> Result<()> {
        if !(self.0)(&ResourceBoundary {
            operation,
            after,
            path: path.into(),
        }) {
            return Err(Error::new(
                "Compose resource operation is unresolved; preserve its evidence",
            ));
        }
        Ok(())
    }

    pub(super) fn around<T>(
        &self,
        operation: ResourceOperation,
        path: &Path,
        action: impl FnOnce() -> Result<T>,
    ) -> Result<T> {
        self.at(operation, false, path)?;
        let result = action()?;
        self.at(operation, true, path)?;
        Ok(result)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(super) struct Identity {
    device: u64,
    inode: u64,
}

impl Identity {
    pub fn of(file: &File) -> Result<Self> {
        let m = file
            .metadata()
            .map_err(|_| Error::new("Cannot inspect compose resource identity"))?;
        Ok(Self {
            device: m.dev(),
            inode: m.ino(),
        })
    }
}

pub(super) struct Directory {
    pub path: PathBuf,
    pub file: File,
    pub observer: ResourceObserver,
}

#[derive(PartialEq, Eq)]
struct Revision {
    identity: Identity,
    length: u64,
    modified: (i64, i64),
    changed: (i64, i64),
}

pub(super) struct ReadRevision {
    metadata: Revision,
    digest: [u8; 32],
}

impl Revision {
    fn of(file: &File) -> Result<Self> {
        // Ownership/type and the revision come from one metadata observation
        // of the descriptor whose bytes are read, never a later pathname open.
        let m = Directory::regular(file)?;
        Ok(Self {
            identity: Identity {
                device: m.dev(),
                inode: m.ino(),
            },
            length: m.len(),
            modified: (m.mtime(), m.mtime_nsec()),
            changed: (m.ctime(), m.ctime_nsec()),
        })
    }
}

pub(super) struct Lease {
    file: File,
    path: PathBuf,
    observer: ResourceObserver,
}

impl Lease {
    pub fn check(&self) -> Result<()> {
        let fd = openat2(
            rustix::fs::CWD,
            &self.path,
            OFlags::RDONLY | OFlags::NONBLOCK | OFlags::CLOEXEC,
            Mode::empty(),
            ResolveFlags::NO_SYMLINKS,
        )
        .map_err(|_| {
            Error::new("Compose lease identity is unavailable; preserve its lock evidence")
        })?;
        if Identity::of(&File::from(fd))? != Identity::of(&self.file)? {
            return Err(Error::new(
                "Compose lease inode was replaced; preserve both owners' evidence",
            ));
        }
        Ok(())
    }

    pub fn release(self) -> Result<()> {
        self.check()?;
        self.observer
            .at(ResourceOperation::Unlock, false, &self.path)?;
        rustix::fs::flock(&self.file, rustix::fs::FlockOperation::Unlock)
            .map_err(|_| Error::new("Compose resource lock release is unresolved"))?;
        self.observer
            .at(ResourceOperation::Unlock, true, &self.path)
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        // CLOEXEC closes only after exec: another thread's child may still
        // share this open description. Explicit unlock avoids a spurious lease
        // extension while that child crosses fork-to-exec. This fallback never
        // certifies resource retirement; normal paths check release explicitly.
        let _ = rustix::fs::flock(&self.file, rustix::fs::FlockOperation::Unlock);
    }
}

fn name(name: &str) -> Result<()> {
    if name.is_empty() || name == "." || name == ".." || name.contains(['/', '\\', '\0']) {
        return Err(Error::new("Invalid private compose resource name"));
    }
    Ok(())
}

impl Directory {
    fn revision(&self, entry: &str) -> Result<Option<Revision>> {
        let file = match openat2(
            &self.file,
            entry,
            OFlags::RDONLY | OFlags::NONBLOCK | OFlags::CLOEXEC,
            Mode::empty(),
            ResolveFlags::BENEATH | ResolveFlags::NO_SYMLINKS | ResolveFlags::NO_XDEV,
        ) {
            Ok(fd) => File::from(fd),
            Err(rustix::io::Errno::NOENT) => return Ok(None),
            Err(_) => {
                return Err(Error::new(
                    "Cannot identify the current compose evidence revision",
                ));
            }
        };
        Ok(Some(Revision::of(&file)?))
    }
    pub fn open(path: &Path, observer: ResourceObserver) -> Result<Self> {
        observer.at(ResourceOperation::Open, false, path)?;
        let fd = openat2(
            rustix::fs::CWD,
            path,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
            ResolveFlags::NO_SYMLINKS,
        )
        .map_err(|_| Error::new("Cannot open the no-follow compose directory"))?;
        observer.at(ResourceOperation::Open, true, path)?;
        Ok(Self {
            path: path.into(),
            file: File::from(fd),
            observer,
        })
    }

    pub fn private(&self) -> Result<()> {
        let m = self
            .file
            .metadata()
            .map_err(|_| Error::new("Cannot inspect private compose storage"))?;
        if !m.is_dir() || m.uid() != rustix::process::geteuid().as_raw() || m.mode() & 0o077 != 0 {
            return Err(Error::new(
                "Compose storage requires an owned private directory",
            ));
        }
        self.check()
    }

    pub fn identity(&self) -> Result<Identity> {
        Identity::of(&self.file)
    }

    pub fn check(&self) -> Result<()> {
        let fd = openat2(
            rustix::fs::CWD,
            &self.path,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
            ResolveFlags::NO_SYMLINKS,
        )
        .map_err(|_| {
            Error::new("Compose directory identity is unavailable; preserve its evidence")
        })?;
        if Identity::of(&File::from(fd))? != self.identity()? {
            return Err(Error::new(
                "Compose directory was replaced; preserve both locations",
            ));
        }
        Ok(())
    }

    pub fn child(&self, child: &str, create: bool) -> Result<Self> {
        name(child)?;
        self.check()?;
        let path = self.path.join(child);
        if create {
            self.observer.at(ResourceOperation::Create, false, &path)?;
            match rustix::fs::mkdirat(&self.file, child, Mode::RUSR | Mode::WUSR | Mode::XUSR) {
                Ok(()) => self.sync()?,
                Err(rustix::io::Errno::EXIST) => (),
                Err(_) => return Err(Error::new("Cannot create private compose storage")),
            }
            self.observer.at(ResourceOperation::Create, true, &path)?;
        }
        let fd = openat2(
            &self.file,
            child,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
            ResolveFlags::BENEATH | ResolveFlags::NO_SYMLINKS | ResolveFlags::NO_XDEV,
        )
        .map_err(|_| Error::new("Cannot open contained private compose storage"))?;
        let result = Self {
            path,
            file: File::from(fd),
            observer: self.observer.clone(),
        };
        result.private()?;
        // A child can be a mount point onto another filesystem; check the
        // handle that will actually hold records and locks (DEC-1070.1).
        super::files::qualified_filesystem(&result.file, "compose storage directory")?;
        self.check()?;
        Ok(result)
    }

    pub fn create_owned(&self, child: &str) -> Result<Self> {
        name(child)?;
        self.check()?;
        let path = self.path.join(child);
        self.observer.at(ResourceOperation::Create, false, &path)?;
        rustix::fs::mkdirat(&self.file, child, Mode::RUSR | Mode::WUSR | Mode::XUSR).map_err(
            |_| Error::new("Private compose resource already exists or cannot be acquired"),
        )?;
        let acquired = self.child(child, false)?;
        self.observer.at(ResourceOperation::Create, true, &path)?;
        acquired.check()?;
        self.sync()?;
        Ok(acquired)
    }

    pub fn sync(&self) -> Result<()> {
        self.observer
            .at(ResourceOperation::Sync, false, &self.path)?;
        self.file
            .sync_all()
            .map_err(|_| Error::new("Compose directory flush is unresolved"))?;
        self.observer
            .at(ResourceOperation::Sync, true, &self.path)?;
        self.check()
    }

    pub fn read(&self, entry: &str, limit: u64) -> Result<Option<Vec<u8>>> {
        Ok(self
            .read_with_revision(entry, limit)?
            .map(|(bytes, _)| bytes))
    }

    pub fn check_revision(&self, entry: &str, expected: Option<&ReadRevision>) -> Result<()> {
        name(entry)?;
        self.check()?;
        let matches = || -> Result<bool> {
            let Some(expected) = expected else {
                return Ok(self.revision(entry)?.is_none());
            };
            let fd = openat2(
                &self.file,
                entry,
                OFlags::RDONLY | OFlags::NONBLOCK | OFlags::CLOEXEC,
                Mode::empty(),
                ResolveFlags::BENEATH | ResolveFlags::NO_SYMLINKS | ResolveFlags::NO_XDEV,
            )
            .map_err(|_| {
                Error::new("Cannot reopen compose evidence for revision verification")
                    .at(&self.path.join(entry))
            })?;
            let mut file = File::from(fd);
            if Revision::of(&file)? != expected.metadata {
                return Ok(false);
            }
            // Fast equal-length writes can share all filesystem timestamps.
            // Stream at most the observed length plus one; metadata alone must
            // never certify bytes from a different content revision.
            let mut reader = Read::by_ref(&mut file).take(expected.metadata.length + 1);
            let mut hasher = Sha256::new();
            let mut buffer = [0; 8192];
            loop {
                let read = reader.read(&mut buffer).map_err(|_| {
                    Error::new("Cannot verify compose evidence contents").at(&self.path.join(entry))
                })?;
                if read == 0 {
                    break;
                }
                hasher.update(&buffer[..read]);
            }
            let digest: [u8; 32] = hasher.finalize().into();
            Ok(digest == expected.digest
                && Revision::of(&file)? == expected.metadata
                && self.revision(entry)?.as_ref() == Some(&expected.metadata))
        };
        if !matches!(matches(), Ok(true)) {
            return Err(Error::new(&format!(
                "Compose evidence at {} changed or is unavailable; preserve it",
                self.path.join(entry).display()
            )));
        }
        self.check()
    }

    pub fn read_with_revision(
        &self,
        entry: &str,
        limit: u64,
    ) -> Result<Option<(Vec<u8>, ReadRevision)>> {
        name(entry)?;
        self.check()?;
        let path = self.path.join(entry);
        self.observer.at(ResourceOperation::Read, false, &path)?;
        let fd = match openat2(
            &self.file,
            entry,
            OFlags::RDONLY | OFlags::NONBLOCK | OFlags::CLOEXEC,
            Mode::empty(),
            ResolveFlags::BENEATH | ResolveFlags::NO_SYMLINKS | ResolveFlags::NO_XDEV,
        ) {
            Ok(fd) => fd,
            Err(rustix::io::Errno::NOENT) => {
                self.check()?;
                self.observer.at(ResourceOperation::Read, true, &path)?;
                self.check_revision(entry, None)?;
                return Ok(None);
            }
            Err(_) => return Err(Error::new("Compose evidence is unreadable; preserve it")),
        };
        let mut file = File::from(fd);
        let revision = Revision::of(&file)?;
        let mut bytes = Vec::new();
        Read::by_ref(&mut file)
            .take(limit + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| Error::new("Cannot read compose evidence"))?;
        if bytes.len() as u64 > limit {
            return Err(Error::new("Compose evidence exceeds its size limit"));
        }
        self.observer.at(ResourceOperation::Read, true, &path)?;
        if Revision::of(&file)? != revision {
            return Err(Error::new(&format!(
                "Compose evidence at {} changed while being read; preserve it",
                path.display()
            )));
        }
        let revision = ReadRevision {
            metadata: revision,
            digest: Sha256::digest(&bytes).into(),
        };
        self.check_revision(entry, Some(&revision))?;
        Ok(Some((bytes, revision)))
    }

    fn regular(file: &File) -> Result<std::fs::Metadata> {
        let m = file
            .metadata()
            .map_err(|_| Error::new("Cannot inspect compose evidence"))?;
        if !m.is_file() || m.nlink() != 1 || m.uid() != rustix::process::geteuid().as_raw() {
            return Err(Error::new(
                "Compose evidence must be an owned single-link regular file",
            ));
        }
        Ok(m)
    }

    pub fn lock(&self, entry: &str) -> Result<Lease> {
        name(entry)?;
        self.check()?;
        let path = self.path.join(entry);
        self.observer.at(ResourceOperation::Open, false, &path)?;
        let file = File::from(
            openat2(
                &self.file,
                entry,
                OFlags::RDWR | OFlags::CREATE | OFlags::NONBLOCK | OFlags::CLOEXEC,
                Mode::RUSR | Mode::WUSR,
                ResolveFlags::BENEATH | ResolveFlags::NO_SYMLINKS | ResolveFlags::NO_XDEV,
            )
            .map_err(|_| Error::new("Cannot open compose ownership evidence"))?,
        );
        Self::regular(&file)?;
        rustix::fs::flock(&file, rustix::fs::FlockOperation::NonBlockingLockExclusive)
            .map_err(|_| Error::new("Another compose operation owns this resource"))?;
        let lease = Lease {
            file,
            path: path.clone(),
            observer: self.observer.clone(),
        };
        self.sync()?;
        self.observer.at(ResourceOperation::Open, true, &path)?;
        self.check()?;
        lease.check()?;
        // Never unlink this inode: another owner must acquire the same lock.
        Ok(lease)
    }

    pub fn names(&self) -> Result<Vec<String>> {
        self.check()?;
        self.observer
            .at(ResourceOperation::Read, false, &self.path)?;
        let mut stream = rustix::fs::Dir::read_from(&self.file)
            .map_err(|_| Error::new("Cannot enumerate compose evidence"))?;
        let mut names = Vec::new();
        while let Some(entry) = stream.read() {
            let entry =
                entry.map_err(|_| Error::new("Cannot finish enumerating compose evidence"))?;
            let value = entry
                .file_name()
                .to_str()
                .map_err(|_| Error::new("Unknown compose evidence name"))?;
            if value != "." && value != ".." {
                names.push(value.to_owned());
            }
            if names.len() > 32768 {
                return Err(Error::new("Too many private compose resources"));
            }
        }
        names.sort();
        self.observer
            .at(ResourceOperation::Read, true, &self.path)?;
        self.check()?;
        Ok(names)
    }

    pub fn save(&self, entry: &str, bytes: &[u8]) -> Result<()> {
        name(entry)?;
        self.check()?;
        let expected = self.revision(entry)?;
        let temporary = format!("pending-{}", random_id()?);
        let path = self.path.join(&temporary);
        self.observer.at(ResourceOperation::Create, false, &path)?;
        let mut file = File::from(
            openat2(
                &self.file,
                &temporary,
                OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::CLOEXEC,
                Mode::RUSR | Mode::WUSR,
                ResolveFlags::BENEATH | ResolveFlags::NO_SYMLINKS | ResolveFlags::NO_XDEV,
            )
            .map_err(|_| Error::new("Cannot create compose persistence evidence"))?,
        );
        self.observer.at(ResourceOperation::Create, true, &path)?;
        self.observer.at(ResourceOperation::Write, false, &path)?;
        file.write_all(bytes)
            .map_err(|_| Error::new("Compose evidence write is unresolved"))?;
        self.observer.at(ResourceOperation::Write, true, &path)?;
        self.observer.at(ResourceOperation::Sync, false, &path)?;
        file.sync_all()
            .map_err(|_| Error::new("Compose evidence flush is unresolved"))?;
        self.observer.at(ResourceOperation::Sync, true, &path)?;
        self.check()?;
        let destination = self.path.join(entry);
        self.observer
            .at(ResourceOperation::Rename, false, &destination)?;
        if self.revision(entry)? != expected {
            return Err(Error::new(
                "Compose evidence changed before replacement; preserve both revisions",
            ));
        }
        rustix::fs::renameat(&self.file, &temporary, &self.file, entry)
            .map_err(|_| Error::new("Compose evidence replacement is unresolved"))?;
        self.observer
            .at(ResourceOperation::Rename, true, &destination)?;
        self.sync()
    }

    pub fn remove(&self, entry: &str) -> Result<()> {
        name(entry)?;
        self.check()?;
        let path = self.path.join(entry);
        let open = || -> Result<Option<File>> {
            match openat2(
                &self.file,
                entry,
                OFlags::RDONLY | OFlags::NONBLOCK | OFlags::CLOEXEC,
                Mode::empty(),
                ResolveFlags::BENEATH | ResolveFlags::NO_SYMLINKS | ResolveFlags::NO_XDEV,
            ) {
                Ok(fd) => {
                    let file = File::from(fd);
                    Self::regular(&file)?;
                    Ok(Some(file))
                }
                Err(rustix::io::Errno::NOENT) => Ok(None),
                Err(_) => Err(Error::new(
                    "Compose evidence cannot be attributed for retirement",
                )),
            }
        };
        let expected = open()?.map(|f| Identity::of(&f)).transpose()?;
        self.observer.at(ResourceOperation::Remove, false, &path)?;
        if open()?.map(|f| Identity::of(&f)).transpose()? != expected {
            return Err(Error::new(
                "Compose evidence was replaced before retirement; preserve it",
            ));
        }
        match rustix::fs::unlinkat(&self.file, entry, AtFlags::empty()) {
            Ok(()) | Err(rustix::io::Errno::NOENT) => (),
            Err(_) => return Err(Error::new("Compose evidence retirement is unresolved")),
        }
        self.observer.at(ResourceOperation::Remove, true, &path)?;
        self.sync()
    }
}

/// Refuses when any directory beneath `directory` is a mount of its own or a
/// symbolic link, walking every existing directory once.
fn same_filesystem_tree(directory: &impl std::os::fd::AsFd, prefix: &str) -> Result<()> {
    let mut stream = rustix::fs::Dir::read_from(directory)
        .map_err(|_| Error::new("Could not enumerate the Git directory"))?;
    while let Some(entry) = stream.read() {
        let entry = entry.map_err(|_| Error::new("Git directory enumeration is incomplete"))?;
        let Ok(name) = entry.file_name().to_str() else {
            return Err(Error::new("A Git directory entry has an unsupported name"));
        };
        if name == "." || name == ".." {
            continue;
        }
        let kind = entry.file_type();
        let relative = if prefix.is_empty() {
            name.to_owned()
        } else {
            format!("{prefix}/{name}")
        };
        if !matches!(
            kind,
            rustix::fs::FileType::Directory
                | rustix::fs::FileType::Symlink
                | rustix::fs::FileType::Unknown
        ) {
            continue;
        }
        match openat2(
            directory,
            name,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
            ResolveFlags::BENEATH | ResolveFlags::NO_SYMLINKS | ResolveFlags::NO_XDEV,
        ) {
            Ok(child) => same_filesystem_tree(&child, &relative)?,
            Err(rustix::io::Errno::XDEV) => {
                return Err(Error::new(&format!(
                    "The Git directory's {relative} is another filesystem mounted inside the \
                     repository; compose writes there and has not qualified it"
                )));
            }
            // Git's files backend follows a link to a directory, so writes
            // beneath one could land anywhere, 9p included: refuse it. A link
            // to a file, or to nothing, is a ref the census reports as
            // evidence, and refusing here would hide that behind a generic
            // refusal. Special entries are not directories Git writes into.
            Err(rustix::io::Errno::LOOP) => {
                if rustix::fs::statat(directory, name, AtFlags::empty()).is_ok_and(|target| {
                    rustix::fs::FileType::from_raw_mode(target.st_mode)
                        == rustix::fs::FileType::Directory
                }) {
                    return Err(Error::new(&format!(
                        "The Git directory's {relative} is a symbolic link to a directory; \
                         Git would write through it, so compose refuses"
                    )));
                }
            }
            Err(_) => {}
        }
    }
    Ok(())
}

pub(super) fn store(common: &Path, observer: ResourceObserver) -> Result<Directory> {
    let common = Directory::open(common, observer)?;
    super::files::qualified_filesystem(&common.file, "Git directory holding compose's records")?;
    // Git writes objects, packs, refs and reflogs anywhere beneath the common
    // directory, through paths no `NO_XDEV` lookup of compose's can cover. So
    // every directory that exists there is reopened without crossing a mount
    // or following a link. One that Git creates later is made on its
    // parent's filesystem, which this walk has checked (DEC-1070.1).
    // Only the trees written during compose are walked: a symlinked hooks
    // directory, for example, is ordinary and compose never writes hooks.
    for tree in ["objects", "refs", "logs", "pbps-compose-v2"] {
        match openat2(
            &common.file,
            tree,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
            ResolveFlags::BENEATH | ResolveFlags::NO_SYMLINKS | ResolveFlags::NO_XDEV,
        ) {
            Ok(fd) => {
                super::files::qualified_filesystem(&fd, &format!("Git directory's {tree}"))?;
                same_filesystem_tree(&fd, tree)?;
            }
            Err(rustix::io::Errno::XDEV) => {
                return Err(Error::new(&format!(
                    "The Git directory's {tree} is another filesystem mounted inside the \
                     repository; compose writes there and has not qualified it"
                )));
            }
            Err(rustix::io::Errno::LOOP) => {
                return Err(Error::new(&format!(
                    "The Git directory's {tree} is a symbolic link; Git would write through \
                     it, so compose refuses"
                )));
            }
            // Absent or unreadable: not a mount. The operations that use it
            // refuse or report it as before.
            Err(_) => {}
        }
    }
    common.child("pbps-compose-v2", true)
}

pub(super) fn refuse_legacy(git: &super::git::Git) -> Result<()> {
    for selector in ["--git-dir", "--git-common-dir"] {
        let directory =
            PathBuf::from(git.line(&["rev-parse", "--path-format=absolute", selector])?);
        let root = Directory::open(&directory, ResourceObserver::default())?;
        match openat2(
            &root.file,
            "pbps-ui",
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
            ResolveFlags::BENEATH | ResolveFlags::NO_SYMLINKS | ResolveFlags::NO_XDEV,
        ) {
            Err(rustix::io::Errno::NOENT) => continue,
            Ok(fd) => {
                if matches!(
                    rustix::fs::statat(&fd, "composing", AtFlags::SYMLINK_NOFOLLOW),
                    Err(rustix::io::Errno::NOENT)
                ) {
                    continue;
                }
                return Err(Error::new(&format!(
                    "Legacy compose evidence at {}: preserve it and use the matching experimental binary or manual recovery before composing",
                    directory.join("pbps-ui/composing").display()
                )));
            }
            Err(_) => {
                return Err(Error::new(&format!(
                    "Cannot inspect legacy compose evidence at {}; preserve it for manual recovery",
                    directory.join("pbps-ui").display()
                )));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn equal_metadata_does_not_certify_different_record_bytes() {
        let path = std::env::temp_dir().join(format!("pbps-read-content-{}", random_id().unwrap()));
        std::fs::create_dir(&path).unwrap();
        let entry = path.join("record.json");
        std::fs::write(&entry, b"before").unwrap();
        let directory = Directory::open(&path, ResourceObserver::default()).unwrap();
        let (_, mut revision) = directory
            .read_with_revision("record.json", 100)
            .unwrap()
            .unwrap();
        directory
            .check_revision("record.json", Some(&revision))
            .unwrap();
        std::fs::write(&entry, b"after!").unwrap();
        // Model timestamp aliasing deterministically: the metadata comparison
        // succeeds, so only the digest can distinguish these equal-length bytes.
        revision.metadata = directory.revision("record.json").unwrap().unwrap();
        assert!(
            directory
                .check_revision("record.json", Some(&revision))
                .is_err()
        );
        assert_eq!(std::fs::read(&entry).unwrap(), b"after!");
        std::fs::remove_dir_all(path).unwrap();
    }

    #[test]
    fn a_record_read_never_certifies_a_later_revision() {
        use std::sync::atomic::{AtomicBool, Ordering};

        for mode in ["replace", "rewrite", "remove", "appear"] {
            let path =
                std::env::temp_dir().join(format!("pbps-read-revision-{}", random_id().unwrap()));
            std::fs::create_dir(&path).unwrap();
            let entry = path.join("record.json");
            if mode != "appear" {
                std::fs::write(&entry, b"before").unwrap();
            }
            let armed = Arc::new(AtomicBool::new(false));
            let enabled = armed.clone();
            let fired = Arc::new(AtomicBool::new(false));
            let flag = fired.clone();
            let target = entry.clone();
            let observer = ResourceObserver::new(move |at| {
                if enabled.load(Ordering::SeqCst)
                    && at.operation == ResourceOperation::Read
                    && at.after
                    && at.path == target
                    && !flag.swap(true, Ordering::SeqCst)
                {
                    match mode {
                        "replace" => {
                            let replacement = target.with_extension("replacement");
                            std::fs::write(&replacement, b"after!").unwrap();
                            std::fs::rename(replacement, &target).unwrap();
                        }
                        "remove" => std::fs::remove_file(&target).unwrap(),
                        _ => std::fs::write(&target, b"after!").unwrap(),
                    }
                }
                true
            });
            let directory = Directory::open(&path, observer).unwrap();
            assert_eq!(
                directory.read("record.json", 100).unwrap(),
                (mode != "appear").then(|| b"before".to_vec())
            );
            armed.store(true, Ordering::SeqCst);
            assert!(directory.read("record.json", 100).is_err(), "{mode}");
            assert!(fired.load(Ordering::SeqCst));
            if mode == "remove" {
                assert!(!entry.exists());
            } else {
                assert_eq!(std::fs::read(entry).unwrap(), b"after!");
            }
            std::fs::remove_dir_all(path).unwrap();
        }
    }

    #[test]
    fn a_shared_open_description_cannot_extend_a_released_resource_lease() {
        let path =
            std::env::temp_dir().join(format!("pbps-resource-lease-{}", random_id().unwrap()));
        std::fs::create_dir(&path).unwrap();
        let root = Directory::open(&path, ResourceObserver::default()).unwrap();
        let lease = root.lock("owner.lock").unwrap();
        assert!(root.lock("owner.lock").is_err());
        // dup shares the same kernel open description as an inherited fork fd.
        let inherited = lease.file.try_clone().unwrap();
        lease.release().unwrap();
        let next = root.lock("owner.lock").unwrap();
        drop(inherited);
        assert!(root.lock("owner.lock").is_err());
        next.release().unwrap();
        assert!(path.join("owner.lock").exists());
        std::fs::remove_dir_all(&path).unwrap();
    }
}

#[cfg(test)]
mod write_tree_tests {
    use super::*;

    fn tree(name: &str) -> PathBuf {
        let root =
            std::env::temp_dir().join(format!("pbps-write-tree-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("refs/heads")).unwrap();
        std::fs::create_dir_all(root.join("elsewhere")).unwrap();
        std::fs::write(root.join("refs/heads/master"), "0\n").unwrap();
        root
    }

    fn walk(root: &Path) -> Result<()> {
        let fd = openat2(
            rustix::fs::CWD,
            root.join("refs"),
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
            ResolveFlags::NO_SYMLINKS,
        )
        .unwrap();
        same_filesystem_tree(&fd, "refs")
    }

    #[test]
    fn a_link_to_a_directory_in_a_write_tree_is_refused_and_a_link_to_a_file_is_not() {
        let root = tree("dir-link");
        assert!(walk(&root).is_ok());
        // A ref that links to another ref, or to nothing, is census evidence.
        std::os::unix::fs::symlink("master", root.join("refs/heads/alias")).unwrap();
        std::os::unix::fs::symlink("gone", root.join("refs/heads/dangling")).unwrap();
        assert!(walk(&root).is_ok());
        // A directory Git would write through, wherever it points.
        std::os::unix::fs::symlink(root.join("elsewhere"), root.join("refs/heads/pbps-compose"))
            .unwrap();
        let refused = walk(&root).unwrap_err().to_string();
        assert!(refused.contains("refs/heads/pbps-compose"), "{refused}");
        assert!(
            refused.contains("symbolic link to a directory"),
            "{refused}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
