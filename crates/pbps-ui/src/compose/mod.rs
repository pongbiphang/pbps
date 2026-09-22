//! Immutable isolated candidates (ADR-0017, DECISIONS 530).
//!
//! Publication adds only private evidence, Git objects and a fresh output ref.
//! The viewer continues to reject writes until #746–#748 qualify delivery.

mod capture;
mod cli;
mod destination;
mod durable;
mod files;
mod git;
mod inventory;
mod process;
mod record;
mod recover;
mod refs;
mod resources;
mod run;
mod transport;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub use cli::Intent;
pub use destination::Destination;
pub use durable::{ResourceBoundary, ResourceObserver, ResourceOperation};
pub use files::Evidence;
pub use recover::{DeliveryState, Details, LocalState, Outcome, Problem, Status};
pub use refs::RefEvidence;
pub use resources::{ResourceReport, ResourceState};
pub use run::{DurableStage, PublicationBoundary, Publications};

#[derive(Debug)]
pub struct Error {
    message: String,
    failure: Failure,
}

#[derive(Debug, PartialEq, Eq)]
enum Failure {
    Uncertain,
    CompletedRejection,
}

impl Error {
    fn new(message: &str) -> Self {
        Self {
            message: message.to_owned(),
            failure: Failure::Uncertain,
        }
    }

    fn rejected(message: &str) -> Self {
        Self {
            message: message.to_owned(),
            failure: Failure::CompletedRejection,
        }
    }

    fn rejection(self) -> Self {
        Self {
            failure: Failure::CompletedRejection,
            ..self
        }
    }
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
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
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
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
    pub attribute_files: BTreeMap<String, Option<Evidence>>,
    pub line_endings: BTreeMap<String, String>,
    pub autocrlf: bool,
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
    repository_identity: record::RepositoryIdentity,
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
    PathsResolved,
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
    alternative_base: Option<String>,
    resource_observer: ResourceObserver,
}

impl Candidates {
    pub fn new(config: Config) -> Self {
        Self::with_resources(config, ResourceObserver::default())
    }

    pub fn with_resources(config: Config, resource_observer: ResourceObserver) -> Self {
        Self {
            config,
            current: None,
            alternative_base: None,
            resource_observer,
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
        if let Some(Stored::Previewed { candidate, .. }) = self.current.take() {
            candidate.workspace.retire_preview()?;
        }
        let candidate = Arc::new(capture::capture(
            &self.config,
            request,
            observer,
            self.resource_observer.clone(),
        )?);
        if self
            .alternative_base
            .as_ref()
            .is_some_and(|base| base != &candidate.preview.base)
        {
            return Err(Error::new(
                "The original base changed; start a separately reviewed workflow",
            ));
        }
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
                    candidate.workspace.retire_preview()?;
                    self.current = None;
                    return Err(Error::new(
                        "The preview clock changed; refresh the candidate",
                    ));
                };
                if age >= Duration::from_secs(24 * 60 * 60) {
                    candidate.workspace.retire_preview()?;
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
        candidate
            .workspace
            .resources
            .confirm(&candidate.preview.operation_id)?;
        self.current = Some(Stored::Confirmed(Arc::clone(&candidate)));
        Ok(candidate)
    }
}
