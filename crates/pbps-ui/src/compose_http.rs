//! The viewer's only write surface: a fixed set of compose actions over the
//! qualified service (ADR-0017, #494). The browser sends intent fields and
//! opaque handles; the tree, manifest, destination and commit stay here.

use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use serde::Deserialize;
use serde::de::DeserializeOwned;

use crate::compose::{
    self, Candidates, DeliveryState, LocalState, Outcome, Problem, Publications, RefEvidence,
    Request, Status,
};

/// Bounds one compose action, including its Git and CLI subprocesses.
const DEADLINE: Duration = Duration::from_secs(120);
/// A request carries intent fields and handles, never file contents.
pub(crate) const BODY_LIMIT: u64 = 64 * 1024;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Handle {
    candidate_id: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Operation {
    operation_id: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Republish {
    operation_id: String,
    generation: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Nothing {}

pub(crate) struct Compose {
    candidates: Candidates,
    project: PathBuf,
    /// The confirmed handle whose last confirmation was refused before
    /// publication: it is confirmed, yet provably has no receipt.
    receiptless: Option<(String, String)>,
}

/// A refusal for the page: an HTTP status and a message that names no
/// credential (compose errors are built from public identity and paths).
pub(crate) type Refusal = (u16, String);

fn parse<T: DeserializeOwned>(body: &[u8]) -> Result<T, Refusal> {
    serde_json::from_slice(body).map_err(|_| (400, "Malformed compose request".to_owned()))
}

fn refused(error: compose::Error) -> Refusal {
    (409, error.to_string())
}

fn encode<T: serde::Serialize>(value: &T) -> Result<Vec<u8>, Refusal> {
    serde_json::to_vec(value).map_err(|_| (500, "Cannot encode the compose result".to_owned()))
}

impl Compose {
    pub(crate) fn new(executable: PathBuf, project: PathBuf) -> Self {
        Self {
            candidates: Candidates::new(compose::Config {
                executable,
                project: project.clone(),
                deadline: DEADLINE,
            }),
            project,
            receiptless: None,
        }
    }

    // Opened per action: the publisher's owner lock is held only while one
    // action runs, so the CLI and another viewer are never locked out.
    fn publisher(&self) -> Result<Publications, Refusal> {
        let repository = compose::source_repository(&self.project, DEADLINE).map_err(refused)?;
        Publications::open(&repository, DEADLINE).map_err(refused)
    }

    pub(crate) fn answer(&mut self, action: &str, body: &[u8]) -> Result<Vec<u8>, Refusal> {
        match action {
            "preview" => {
                let request: Request = parse(body)?;
                encode(
                    &self
                        .candidates
                        .preview(request, SystemTime::now())
                        .map_err(refused)?,
                )
            }
            "confirm" => {
                let Handle { candidate_id } = parse(body)?;
                // The publisher and its owner lock come first. If another
                // compose holds it, the reviewed handle is not consumed and
                // the page gets a definite refusal it may confirm again.
                let mut publisher = match self.publisher() {
                    Ok(publisher) => publisher,
                    Err(refusal) => {
                        // Only a handle known to have no receipt may be told
                        // "not attempted"; any other confirmed handle may
                        // have one, so it stays unavailable until readable.
                        let receiptless = self
                            .receiptless
                            .as_ref()
                            .filter(|(handle, _)| *handle == candidate_id)
                            .map(|(_, operation)| operation.as_str());
                        let Some(operation) = self
                            .candidates
                            .unconfirmed_operation(&candidate_id)
                            .or(receiptless)
                        else {
                            return Err(refusal);
                        };
                        return encode(&Outcome {
                            status: Status::Refused,
                            operation_id: operation.to_owned(),
                            details: None,
                            local: LocalState::NotAttempted,
                            local_evidence: RefEvidence::Absent,
                            remote: DeliveryState::NotAttempted,
                            remote_evidence: None,
                            problem: Some(Problem::RepositoryUnavailable),
                            // The sealed preview still owns private resources.
                            cleanup_pending: true,
                        });
                    }
                };
                let candidate = self
                    .candidates
                    .confirm(&candidate_id, SystemTime::now())
                    .map_err(refused)?;
                let mut outcome = publisher.confirm(&candidate);
                // A refused confirmation persisted nothing (run.rs refuses
                // only before the durable commit intent); any other status
                // may have written a receipt.
                self.receiptless = (outcome.status == Status::Refused)
                    .then(|| (candidate_id.clone(), outcome.operation_id.clone()));
                // Stale authority is never cured by reconfirming, so release
                // the handle for a fresh preview. The outcome was computed
                // while the resources were still confirmed, so its cleanup
                // flag follows the release: done, or retried by that preview.
                if outcome.status == Status::Refused && compose::stale_refusal(outcome.problem) {
                    outcome.cleanup_pending = self
                        .candidates
                        .release_stale(&candidate_id, &outcome)
                        .is_err();
                }
                encode(&outcome)
            }
            "list" => {
                let Nothing {} = parse(body)?;
                encode(&self.publisher()?.list().map_err(refused)?)
            }
            "recover" => {
                let Operation { operation_id } = parse(body)?;
                encode(&self.publisher()?.recover(&operation_id))
            }
            "retry" => {
                let Operation { operation_id } = parse(body)?;
                encode(&self.publisher()?.retry(&operation_id))
            }
            "republish" => {
                let Republish {
                    operation_id,
                    generation,
                } = parse(body)?;
                encode(&self.publisher()?.republish(&operation_id, &generation))
            }
            "alternative" => {
                let Operation { operation_id } = parse(body)?;
                self.publisher()?
                    .start_alternative(&mut self.candidates, &operation_id)
                    .map_err(refused)?;
                encode(&serde_json::json!({}))
            }
            _ => Err((404, "Unknown compose action".to_owned())),
        }
    }
}
