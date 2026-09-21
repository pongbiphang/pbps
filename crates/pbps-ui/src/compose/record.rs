//! Compact operational evidence. No source placement or authentication data.

use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::{Candidate, Destination, Error, Result, SigningPolicy, git::Git, random_id};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(super) struct RepositoryIdentity {
    pub source: PathBuf,
    pub common: PathBuf,
    source_device: u64,
    source_inode: u64,
    common_device: u64,
    common_inode: u64,
}

impl RepositoryIdentity {
    pub fn capture(git: &Git) -> Result<Self> {
        let source = git
            .root
            .canonicalize()
            .map_err(|_| Error::new("Cannot identify the source repository"))?;
        let common = PathBuf::from(git.line(&[
            "rev-parse",
            "--path-format=absolute",
            "--git-common-dir",
        ])?);
        let common = common
            .canonicalize()
            .map_err(|_| Error::new("Cannot identify the Git directory"))?;
        let a = fs::metadata(&source)
            .map_err(|_| Error::new("Cannot inspect the source repository"))?;
        let b =
            fs::metadata(&common).map_err(|_| Error::new("Cannot inspect the Git directory"))?;
        if !a.is_dir() || !b.is_dir() {
            return Err(Error::new("Compose requires repository directories"));
        }
        Ok(Self {
            source,
            common,
            source_device: a.dev(),
            source_inode: a.ino(),
            common_device: b.dev(),
            common_inode: b.ino(),
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(super) struct Description {
    pub operation_id: String,
    pub candidate_id: String,
    pub output_ref: String,
    pub binding: String,
    pub base: String,
    pub tree: String,
    pub repository: RepositoryIdentity,
    pub project: String,
    pub remote: String,
    pub remote_base_ref: String,
    pub destination: Destination,
    pub signing: SigningPolicy,
}

impl Description {
    pub fn candidate(candidate: &Candidate) -> Self {
        let p = &candidate.preview;
        Self {
            operation_id: p.operation_id.clone(),
            candidate_id: p.candidate_id.clone(),
            output_ref: p.output_ref.clone(),
            binding: p.binding.clone(),
            base: p.base.clone(),
            tree: p.tree.clone(),
            repository: candidate.repository_identity.clone(),
            project: candidate.project.clone(),
            remote: candidate.request.remote.clone(),
            remote_base_ref: candidate.request.remote_base_ref.clone(),
            destination: p.destination.clone(),
            signing: p.signing.clone(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "phase", rename_all = "snake_case", deny_unknown_fields)]
pub(super) enum RemotePhase {
    Unattempted,
    Attempted { generation: String },
    Delivered { generation: String },
}

impl RemotePhase {
    pub fn generation(&self) -> Option<&str> {
        match self {
            Self::Unattempted => None,
            Self::Attempted { generation } | Self::Delivered { generation } => Some(generation),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "phase", rename_all = "snake_case", deny_unknown_fields)]
pub(super) enum Phase {
    Preparing,
    Prepared { commit: String },
    LocalAttempt { commit: String },
    LocalPublished { commit: String, remote: RemotePhase },
}

impl Phase {
    pub fn commit(&self) -> Option<&str> {
        match self {
            Self::Preparing => None,
            Self::Prepared { commit }
            | Self::LocalAttempt { commit }
            | Self::LocalPublished { commit, .. } => Some(commit),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(super) struct Record {
    version: u32,
    pub description: Description,
    pub state: Phase,
}

impl Record {
    pub fn new(description: Description) -> Self {
        Self {
            version: 1,
            description,
            state: Phase::Preparing,
        }
    }
    fn validate(&self, name: &str) -> Result<()> {
        let d = &self.description;
        if self.version != 1
            || d.operation_id != name
            || !identity(name)
            || !identity(&d.candidate_id)
            || !identity(&d.binding)
            || d.output_ref != format!("refs/heads/pbps-compose/{name}")
            || !oid(&d.base)
            || !oid(&d.tree)
            || d.base.len() != d.tree.len()
            || self
                .state
                .commit()
                .is_some_and(|c| !oid(c) || c.len() != d.base.len())
            || !d.repository.source.is_absolute()
            || !d.repository.common.is_absolute()
            || (!d.project.is_empty() && super::files::path(&d.project).is_err())
            || !d.remote_base_ref.starts_with("refs/heads/")
            || d.remote_base_ref.chars().any(char::is_control)
            || super::destination::validate(&d.destination).is_err()
            || d.remote.is_empty()
            || !d
                .remote
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"._-".contains(&c))
        {
            return Err(Error::new(
                "The compose receipt is invalid; preserve it for diagnosis",
            ));
        }
        if let Phase::LocalPublished { remote, .. } = &self.state
            && remote.generation().is_some_and(|g| !identity(g))
        {
            return Err(Error::new("The compose delivery generation is invalid"));
        }
        Ok(())
    }
}

pub(super) fn identity(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
pub(super) fn oid(value: &str) -> bool {
    matches!(value.len(), 40 | 64)
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

pub(super) struct Records {
    pub root: PathBuf,
    _lock: File,
}

impl Records {
    pub fn open(common: &Path) -> Result<Self> {
        let root = common.join("pbps-compose-v2");
        match DirBuilder::new().mode(0o700).create(&root) {
            Ok(()) => (),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => (),
            Err(_) => return Err(Error::new("Cannot create the private compose records")),
        }
        let metadata = fs::symlink_metadata(&root)
            .map_err(|_| Error::new("Cannot inspect the compose records"))?;
        if !metadata.is_dir()
            || metadata.uid() != rustix::process::geteuid().as_raw()
            || metadata.mode() & 0o077 != 0
        {
            return Err(Error::new(
                "Compose records require an owned private directory",
            ));
        }
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32)
            .open(root.join("owner.lock"))
            .map_err(|_| Error::new("Cannot open compose ownership evidence"))?;
        if !lock
            .metadata()
            .map_err(|_| Error::new("Cannot inspect compose ownership"))?
            .is_file()
        {
            return Err(Error::new(
                "Compose ownership evidence is not a regular file",
            ));
        }
        rustix::fs::flock(&lock, rustix::fs::FlockOperation::NonBlockingLockExclusive)
            .map_err(|_| Error::new("Another compose publisher owns this repository"))?;
        // The lock file remains in place. Unlinking a locked inode would allow
        // another publisher to lock a different inode under the same name.
        Ok(Self { root, _lock: lock })
    }

    pub fn load(&self, id: &str) -> Result<Option<Record>> {
        if !identity(id) {
            return Err(Error::new("Invalid compose operation identity"));
        }
        let mut file = match OpenOptions::new()
            .read(true)
            .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32)
            .open(self.root.join(format!("{id}.json")))
        {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(_) => {
                return Err(Error::new(
                    "The compose receipt is unreadable; preserve its evidence",
                ));
            }
        };
        if !file
            .metadata()
            .map_err(|_| Error::new("Cannot inspect a compose receipt"))?
            .is_file()
        {
            return Err(Error::new("A compose receipt is not a regular file"));
        }
        let mut bytes = Vec::new();
        Read::by_ref(&mut file)
            .take(1024 * 1024 + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| Error::new("Cannot read a compose receipt"))?;
        if bytes.len() > 1024 * 1024 {
            return Err(Error::new("Compose receipt exceeds its size limit"));
        }
        let record: Record = serde_json::from_slice(&bytes)
            .map_err(|_| Error::new("The compose receipt is invalid; preserve it for diagnosis"))?;
        record.validate(id)?;
        Ok(Some(record))
    }

    pub fn list(&self) -> Result<Vec<Record>> {
        let mut ids = Vec::new();
        for entry in
            fs::read_dir(&self.root).map_err(|_| Error::new("Cannot enumerate compose receipts"))?
        {
            let name = entry
                .map_err(|_| Error::new("Cannot enumerate compose receipts"))?
                .file_name();
            let name = name
                .to_str()
                .ok_or_else(|| Error::new("Invalid compose record name"))?;
            if let Some(id) = name.strip_suffix(".json") {
                ids.push(id.to_owned());
            }
        }
        ids.sort();
        ids.into_iter()
            .map(|id| {
                self.load(&id)?
                    .ok_or_else(|| Error::new("A compose receipt disappeared during discovery"))
            })
            .collect()
    }

    pub fn save(&self, record: &Record) -> Result<()> {
        record.validate(&record.description.operation_id)?;
        let bytes = serde_json::to_vec(record)
            .map_err(|_| Error::new("Cannot encode a compose receipt"))?;
        let temporary = self.root.join(format!("pending-{}", random_id()?));
        let result = (|| -> std::io::Result<()> {
            let mut file = OpenOptions::new()
                .create_new(true)
                .write(true)
                .mode(0o600)
                .open(&temporary)?;
            file.write_all(&bytes)?;
            file.sync_all()?;
            fs::rename(
                &temporary,
                self.root
                    .join(format!("{}.json", record.description.operation_id)),
            )?;
            File::open(&self.root)?.sync_all()
        })();
        // A failure after rename can already have installed the new phase.
        // Callers reconcile disk evidence and never infer rollback permission.
        result.map_err(|_| Error::new("Compose receipt persistence is unresolved"))
    }
}
