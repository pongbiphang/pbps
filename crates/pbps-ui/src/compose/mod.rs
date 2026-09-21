//! Immutable isolated candidates (ADR-0017, DECISIONS 530).
//!
//! This backend has no source placement, user-index installation or publisher.
//! The viewer continues to reject writes until #746–#748 qualify delivery.

mod capture;
mod cli;
mod destination;
mod files;
mod git;
mod process;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub use cli::Intent;
pub use destination::Destination;
pub use files::Evidence;

#[derive(Debug)]
pub struct Error(String);

impl Error {
    fn new(message: &str) -> Self {
        Self(message.to_owned())
    }
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for Error {}
type Result<T> = std::result::Result<T, Error>;

fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn random_id() -> Result<String> {
    let mut bytes = [0_u8; 32];
    let mut remaining = bytes.as_mut_slice();
    while !remaining.is_empty() {
        let n = rustix::rand::getrandom(&mut *remaining, rustix::rand::GetRandomFlags::empty())
            .map_err(|_| Error::new("Could not allocate a compose identity"))?;
        if n == 0 {
            return Err(Error::new("Could not allocate a compose identity"));
        }
        remaining = &mut remaining[n..];
    }
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub intent: Intent,
    pub message: String,
    pub remote: String,
    pub remote_base_ref: String,
}

/// Only public requirements/selectors, never helper commands or secrets.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct SigningPolicy {
    required: bool,
    format: Option<String>,
    key: Option<String>,
}

/// Evidence comes from the same bytes used to build the snapshot. `None`
/// means observed absence; unreadable inputs never create an entry.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Manifest {
    pub inputs: BTreeMap<String, Option<Evidence>>,
    pub attributes: BTreeMap<String, Vec<(String, String)>>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Preview {
    pub candidate_id: String,
    pub operation_id: String,
    pub output_ref: String,
    pub base: String,
    pub tree: String,
    pub binding: String,
    pub diff: String,
    pub destination: Destination,
    pub signing: SigningPolicy,
}

/// Not deserializable: a browser cannot supply a tree, evidence or destination.
pub struct Candidate {
    preview: Preview,
    request: Request,
    manifest: Manifest,
    source: PathBuf,
    project: String,
    workspace: capture::Workspace,
}

impl Candidate {
    pub fn preview(&self) -> &Preview {
        &self.preview
    }
    pub fn request(&self) -> &Request {
        &self.request
    }
    pub fn manifest(&self) -> &Manifest {
        &self.manifest
    }
    pub fn repository(&self) -> &std::path::Path {
        &self.source
    }
    pub fn project(&self) -> &str {
        &self.project
    }
    pub fn snapshot_repository(&self) -> PathBuf {
        self.workspace.path.join("repository")
    }
}

/// Minimal deterministic scheduling boundaries used by the real capture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureBoundary {
    InputsRead,
    IntentRecorded,
    TreeBuilt,
    InputsChecked,
}

pub struct Config {
    pub executable: PathBuf,
    pub project: PathBuf,
    pub deadline: Duration,
}

enum Stored {
    Previewed {
        candidate: Arc<Candidate>,
        created: SystemTime,
    },
    Confirmed(Arc<Candidate>),
}

/// One browser workflow. Refresh invalidates the previous handle even when the
/// replacement fails; confirmed operations belong to the publisher lifecycle.
pub struct Candidates {
    config: Config,
    current: Option<Stored>,
}

impl Candidates {
    pub fn new(config: Config) -> Self {
        Self {
            config,
            current: None,
        }
    }

    pub fn preview(&mut self, request: Request, now: SystemTime) -> Result<Preview> {
        self.preview_observed(request, now, &|_| {})
    }

    pub fn preview_observed(
        &mut self,
        request: Request,
        now: SystemTime,
        observer: &dyn Fn(CaptureBoundary),
    ) -> Result<Preview> {
        if matches!(self.current, Some(Stored::Confirmed(_))) {
            return Err(Error::new(
                "This workflow already confirmed an operation; continue its result before starting another",
            ));
        }
        self.current = None;
        let candidate = Arc::new(capture::capture(&self.config, request, observer)?);
        let preview = candidate.preview.clone();
        self.current = Some(Stored::Previewed {
            candidate,
            created: now,
        });
        Ok(preview)
    }

    pub fn confirm(&mut self, candidate_id: &str, now: SystemTime) -> Result<Arc<Candidate>> {
        let current = self
            .current
            .as_ref()
            .ok_or_else(|| Error::new("Preview the candidate before confirming"))?;
        let candidate = match current {
            Stored::Previewed { candidate, created } => {
                let Ok(age) = now.duration_since(*created) else {
                    self.current = None;
                    return Err(Error::new(
                        "The preview clock changed; refresh the candidate",
                    ));
                };
                if age >= Duration::from_secs(24 * 60 * 60) {
                    self.current = None;
                    return Err(Error::new(
                        "The preview expired; refresh it before confirming",
                    ));
                }
                Arc::clone(candidate)
            }
            Stored::Confirmed(candidate) => Arc::clone(candidate),
        };
        if candidate.preview.candidate_id != candidate_id {
            return Err(Error::new(
                "The candidate is unknown or was replaced; refresh the preview",
            ));
        }
        self.current = Some(Stored::Confirmed(Arc::clone(&candidate)));
        Ok(candidate)
    }
}
