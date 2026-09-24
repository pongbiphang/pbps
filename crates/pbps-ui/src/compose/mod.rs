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
    /// Refused with no publication or receipt for the handle. Its private
    /// resources may have been retired, but nothing was committed or pushed.
    BeforeTransition,
}

impl Error {
    fn new(message: &str) -> Self {
        Self {
            message: message.to_owned(),
            failure: Failure::Uncertain,
        }
    }

    fn definite(message: &str) -> Self {
        Self {
            message: message.to_owned(),
            failure: Failure::BeforeTransition,
        }
    }

    /// True only for a refusal known to precede publication: no receipt,
    /// commit or pushed branch exists for the handle, so a caller may report
    /// "nothing was published". Private resource retirement may already have
    /// happened; a resource-persistence failure is never definite.
    pub fn is_definite(&self) -> bool {
        self.failure == Failure::BeforeTransition
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
    /// Handles this workflow gave up while provably unpublished: replaced,
    /// withdrawn, expired or released before any receipt. Only these may be
    /// refused as "nothing was published"; any other unknown handle may be
    /// an earlier confirmed result and stays uncertain.
    unpublished: std::collections::BTreeSet<String>,
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
            unpublished: std::collections::BTreeSet::new(),
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
        self.release_if_spent();
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
            self.unpublished
                .insert(candidate.preview.candidate_id.clone());
            self.current = None;
        }
        if matches!(self.current, Some(Stored::Confirmed(_))) {
            return Err(Error::new(
                "This workflow already confirmed an operation; continue its result before starting another",
            ));
        }
        if let Some(Stored::Previewed { candidate, .. }) = self.current.take() {
            candidate.workspace.retire_preview()?;
            self.unpublished
                .insert(candidate.preview.candidate_id.clone());
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
        self.unpublished
            .insert(candidate.preview.candidate_id.clone());
        self.current = None;
        Ok(())
    }

    /// Whether this workflow was explicitly authorized to start another
    /// candidate from `base` (the alternative action).
    pub fn admits_sibling_of(&self, base: &str) -> bool {
        self.alternative_base.as_deref() == Some(base)
    }

    /// Retires the workflow's just-sealed preview when a caller refuses to
    /// hand it out; its handle never reached the page.
    pub fn withdraw(&mut self, candidate_id: &str) -> Result<()> {
        let Some(Stored::Previewed { candidate, .. }) = &self.current else {
            return Err(Error::new("No unconfirmed preview to withdraw"));
        };
        if candidate.preview.candidate_id != candidate_id {
            return Err(Error::new("The preview to withdraw was replaced"));
        }
        candidate.workspace.retire_preview()?;
        self.unpublished
            .insert(candidate.preview.candidate_id.clone());
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
            // Only an unconfirmed preview is known to be unpublished; a
            // forgotten confirmation may have delivered.
            if let Some(Stored::Previewed { candidate, .. }) = &self.current {
                self.unpublished
                    .insert(candidate.preview.candidate_id.clone());
            }
            self.current = None;
            self.alternative_base = None;
        }
    }

    /// Whether this workflow gave `candidate_id` up while it was provably
    /// unpublished (replaced, withdrawn, expired or released). Such a handle
    /// has no receipt anywhere, so a caller may refuse it definitely
    /// without reading the publications.
    pub fn is_recorded_unpublished(&self, candidate_id: &str) -> bool {
        self.unpublished.contains(candidate_id)
    }

    /// Another viewer over the same repository may have retired the held
    /// operation's resources: an expired preview, or a forgotten receipt
    /// (#867). Spent is terminal, so the workflow is released exactly as if
    /// this viewer had retired it. An unreadable report changes nothing; the
    /// ordinary path then refuses with the evidence preserved.
    pub fn release_if_spent(&mut self) {
        let held = match &self.current {
            Some(Stored::Previewed { candidate, .. }) | Some(Stored::Confirmed(candidate)) => {
                candidate
            }
            Some(Stored::Releasing(_)) | None => return,
        };
        let operation = held.preview.operation_id.clone();
        let spent = held
            .workspace
            .resources
            .report(&operation)
            .is_ok_and(|report| report.state == ResourceState::Spent);
        if spent {
            self.released(&operation);
        }
    }

    pub fn confirm(&mut self, candidate_id: &str, now: SystemTime) -> Result<Arc<Candidate>> {
        self.release_if_spent();
        // A handle that is not the current one is definite only if this
        // workflow recorded giving it up unpublished; otherwise it may be an
        // earlier confirmed result (for example before an alternative).
        let elsewhere = |unpublished: &std::collections::BTreeSet<String>| {
            let message = "The candidate is unknown or was replaced; refresh the preview";
            if unpublished.contains(candidate_id) {
                Error::definite(message)
            } else {
                Error::new(message)
            }
        };
        let candidate = match self.current.as_ref() {
            None => return Err(elsewhere(&self.unpublished)),
            Some(Stored::Releasing(candidate)) => {
                return Err(if candidate.preview.candidate_id == candidate_id {
                    Error::definite("This candidate was refused and released; preview again")
                } else {
                    elsewhere(&self.unpublished)
                });
            }
            Some(Stored::Confirmed(candidate)) | Some(Stored::Previewed { candidate, .. })
                if candidate.preview.candidate_id != candidate_id =>
            {
                return Err(elsewhere(&self.unpublished));
            }
            Some(Stored::Previewed { candidate, created }) => {
                let candidate = Arc::clone(candidate);
                let expired = match now.duration_since(*created) {
                    Ok(age) if age < Duration::from_secs(24 * 60 * 60) => None,
                    Ok(_) => Some("The preview expired; refresh it before confirming"),
                    Err(_) => Some("The preview clock changed; refresh the candidate"),
                };
                if let Some(message) = expired {
                    candidate.workspace.retire_preview()?;
                    self.unpublished
                        .insert(candidate.preview.candidate_id.clone());
                    self.current = None;
                    return Err(Error::definite(message));
                }
                candidate
            }
            Some(Stored::Confirmed(candidate)) => Arc::clone(candidate),
        };
        // A resource failure may follow its durable Confirmed write, so it
        // stays uncertain: the handle and its evidence are preserved.
        candidate
            .workspace
            .resources
            .confirm(&candidate.preview.operation_id)?;
        self.current = Some(Stored::Confirmed(Arc::clone(&candidate)));
        Ok(candidate)
    }
}
