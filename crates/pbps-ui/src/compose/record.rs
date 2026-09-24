//! Compact operational evidence. No source placement or authentication data.

use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::{
    Candidate, Destination, Error, Result, SigningPolicy,
    durable::{self, Directory, ReadRevision, ResourceObserver},
    git::Git,
};

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
    /// Filtering is only permitted for a valid different source in this common
    /// store. A changed inode at the same path (or a moved same inode) is not an
    /// unrelated worktree and must remain visible as unresolved evidence.
    pub fn source_scope(&self, current: &Self) -> Result<bool> {
        if self.common != current.common
            || self.common_device != current.common_device
            || self.common_inode != current.common_inode
            || !self.source.is_absolute()
        {
            return Err(Error::new(
                "Compose evidence does not match this Git common directory",
            ));
        }
        let path = self.source == current.source;
        let inode = self.source_device == current.source_device
            && self.source_inode == current.source_inode;
        match (path, inode) {
            (true, true) => Ok(true),
            (false, false) => Ok(false),
            _ => Err(Error::new(
                "Compose source identity changed; preserve its evidence",
            )),
        }
    }

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
    CommitKnown { commit: String },
    Prepared { commit: String },
    LocalAttempt { commit: String },
    LocalPublished { commit: String, remote: RemotePhase },
}

impl Phase {
    pub fn commit(&self) -> Option<&str> {
        match self {
            Self::Preparing => None,
            Self::CommitKnown { commit }
            | Self::Prepared { commit }
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
    pub directory: Directory,
    _lock: super::durable::Lease,
}

impl Records {
    pub fn open(common: &Path, observer: ResourceObserver) -> Result<Self> {
        let directory = durable::store(common, observer)?;
        let lock = directory.lock("owner.lock")?;
        let records = Self {
            root: directory.path.clone(),
            directory,
            _lock: lock,
        };
        records.names()?;
        Ok(records)
    }

    fn names(&self) -> Result<Vec<String>> {
        self._lock.check()?;
        let mut ids = Vec::new();
        for name in self.directory.names()? {
            match name.as_str() {
                "owner.lock" | "resources.lock" | "no-hooks" | "resources" | "snapshots" => (),
                _ if name.strip_suffix(".json").is_some_and(identity) => {
                    ids.push(name.trim_end_matches(".json").to_owned());
                }
                _ => {
                    return Err(Error::new(
                        "Unknown compose evidence must be preserved and resolved before publication",
                    ));
                }
            }
        }
        Ok(ids)
    }

    pub fn load(&self, id: &str) -> Result<Option<Record>> {
        self.names()?;
        Ok(self.load_in_pass(id)?.map(|(record, _)| record))
    }

    // The caller brackets a batch with complete namespace/lease checks;
    // direct recovery reads retain their own fresh names() check above.
    fn load_in_pass(&self, id: &str) -> Result<Option<(Record, ReadRevision)>> {
        if !identity(id) {
            return Err(Error::new("Invalid compose operation identity"));
        }
        let path = self.directory.path.join(format!("{id}.json"));
        let Some((bytes, revision)) = self
            .directory
            .read_with_revision(&format!("{id}.json"), 1024 * 1024)
            .map_err(|error| error.at(&path))?
        else {
            return Ok(None);
        };
        let record: Record = serde_json::from_slice(&bytes).map_err(|_| {
            Error::new("The compose receipt is invalid; preserve it for diagnosis").at(&path)
        })?;
        record.validate(id).map_err(|error| error.at(&path))?;
        Ok(Some((record, revision)))
    }

    pub fn list(&self) -> Result<Vec<Record>> {
        let names = self.names()?;
        let records = names
            .iter()
            .map(|id| {
                self.load_in_pass(id)?.ok_or_else(|| {
                    Error::new(
                        "A compose receipt disappeared during discovery; preserve its evidence",
                    )
                    .at(&self.directory.path.join(format!("{id}.json")))
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let closing = self.names()?;
        if closing != names {
            // Name each receipt that appeared or disappeared since the pass.
            let changed = names
                .iter()
                .filter(|id| !closing.contains(id))
                .chain(closing.iter().filter(|id| !names.contains(id)))
                .map(|id| {
                    self.directory
                        .path
                        .join(format!("{id}.json"))
                        .display()
                        .to_string()
                })
                .collect::<Vec<_>>();
            return Err(Error::new(&format!(
                "Compose receipt evidence changed during discovery: {}; preserve it",
                changed.join(", ")
            )));
        }
        for (id, (_, revision)) in names.iter().zip(&records) {
            self.directory
                .check_revision(&format!("{id}.json"), Some(revision))?;
        }
        self._lock.check()?;
        Ok(records.into_iter().map(|(record, _)| record).collect())
    }

    pub fn save(&self, record: &Record) -> Result<()> {
        record.validate(&record.description.operation_id)?;
        let bytes = serde_json::to_vec(record)
            .map_err(|_| Error::new("Cannot encode a compose receipt"))?;
        self.directory
            .save(&format!("{}.json", record.description.operation_id), &bytes)
    }
}
