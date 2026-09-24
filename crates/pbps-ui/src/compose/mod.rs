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

/// A refusal that reconfirming can never cure: the reviewed base,
/// destination, signing policy or repository no longer holds.
pub fn stale_refusal(problem: Option<Problem>) -> bool {
    matches!(
        problem,
        Some(
            Problem::RemoteBaseChanged
                | Problem::DestinationChanged
                | Problem::SigningChanged
                | Problem::RepositoryChanged
        )
    )
}

/// The source checkout that contains `project`, found the way capture finds
/// it: publication identity is the repository root, never a subdirectory.
pub fn source_repository(project: &std::path::Path, deadline: Duration) -> Result<PathBuf> {
    let selected = project
        .canonicalize()
        .map_err(|_| Error::new("Could not locate the selected project"))?;
    let discovery = git::Git {
        root: selected,
        hooks: PathBuf::from("/dev/null"),
        deadline,
    };
    Ok(PathBuf::from(
        discovery.line(&["rev-parse", "--show-toplevel"])?,
    ))
}

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

    /// Names the evidence a refusal is about, keeping its failure kind, so a
    /// caller holding several records can tell which one to preserve (#810).
    fn at(self, path: &std::path::Path) -> Self {
        Self {
            message: format!("{} ({})", self.message, path.display()),
            ..self
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
    /// Refused for stale authority; its private retirement began but may
    /// not have finished. Every later preview retries it first.
    Releasing(Arc<Candidate>),
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
        if let Some(Stored::Releasing(candidate)) = &self.current {
            candidate
                .workspace
                .resources
                .retire(&candidate.preview.operation_id, false)
                .map_err(|error| {
                    Error::new(&format!(
                        "The refused candidate's private resources are still retiring: {error}; preserve them and preview again"
                    ))
                })?;
            self.current = None;
        }
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
            let refusal = "The original base changed; start a separately reviewed workflow";
            // Capture has already sealed this preview. Use its acknowledged
            // discard path and ordinary spent protection (DECISIONS 535).
            if let Err(retirement) = candidate.workspace.retire_preview() {
                return Err(Error::new(&format!(
                    "{refusal}; private retirement remains pending: {retirement}"
                )));
            }
            return Err(Error::new(refusal));
        }
        let preview = candidate.preview.clone();
        self.current = Some(Stored::Previewed {
            candidate,
            created: now,
        });
        Ok(preview)
    }

    /// The operation behind a handle that is previewed but not yet
    /// confirmed: only then can a caller that cannot publish say definitely
    /// that nothing was attempted. A confirmed handle may already have a
    /// receipt, which must be read, not assumed absent.
    pub fn unconfirmed_operation(&self, candidate_id: &str) -> Option<&str> {
        let Some(Stored::Previewed { candidate, .. }) = self.current.as_ref() else {
            return None;
        };
        (candidate.preview.candidate_id == candidate_id)
            .then_some(candidate.preview.operation_id.as_str())
    }

    /// Releases a confirmed candidate whose publication was refused before
    /// any receipt because its reviewed authority went stale (a moved base,
    /// changed destination, signing policy or repository). Reconfirming it
    /// can never succeed, so its private resources retire through the
    /// ordinary path and the workflow accepts a fresh preview.
    pub fn release_stale(&mut self, candidate_id: &str, outcome: &Outcome) -> Result<()> {
        let Some(Stored::Confirmed(candidate)) = &self.current else {
            return Err(Error::new("No confirmed candidate to release"));
        };
        let stale = stale_refusal(outcome.problem);
        if candidate.preview.candidate_id != candidate_id
            || outcome.operation_id != candidate.preview.operation_id
            || outcome.status != Status::Refused
            || outcome.details.as_ref().is_some_and(|d| d.commit.is_some())
            || !stale
        {
            return Err(Error::new(
                "Only a stale pre-publication refusal releases its candidate",
            ));
        }
        // Recorded before retiring, so a failed retirement is retried by the
        // next preview instead of leaving a confirmed handle nothing clears.
        let candidate = Arc::clone(candidate);
        self.current = Some(Stored::Releasing(Arc::clone(&candidate)));
        candidate
            .workspace
            .resources
            .retire(&candidate.preview.operation_id, false)?;
        self.current = None;
        Ok(())
    }

    /// The workflow's own operation was retired outside it: its receipt
    /// was forgotten, or its expired preview was retired. It has no result
    /// to continue and no alternative to start from, so the workflow, and
    /// any alternative-base pin it carried, accepts a fresh preview.
    /// Another operation leaves the workflow untouched.
    pub fn released(&mut self, operation_id: &str) {
        let owns = match &self.current {
            Some(Stored::Previewed { candidate, .. })
            | Some(Stored::Confirmed(candidate))
            | Some(Stored::Releasing(candidate)) => candidate.preview.operation_id == operation_id,
            None => false,
        };
        if owns {
            self.current = None;
            self.alternative_base = None;
        }
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
            Stored::Releasing(_) => {
                return Err(Error::new(
                    "This candidate was refused and released; preview again",
                ));
            }
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
