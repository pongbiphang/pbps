//! One classifier for live errors, repeated requests and startup recovery.

use serde::Serialize;

use super::{
    Destination,
    record::{Phase, Record, RemotePhase},
    refs::RefEvidence,
};

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Refused,
    PreparationUnknown,
    Prepared,
    PublicationUnknown,
    Published,
    Delivered,
    RecoveryRequired,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LocalState {
    NotAttempted,
    Unknown,
    Present,
    Changed,
    Unavailable,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryState {
    NotAttempted,
    Unknown,
    Delivered,
    Changed,
    Unavailable,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Problem {
    ReceiptUnavailable,
    RepositoryChanged,
    RepositoryUnavailable,
    DestinationChanged,
    SigningChanged,
    SigningUnavailable,
    IdentityUnavailable,
    RemoteBaseChanged,
    ObjectImportFailed,
    CommitUnavailable,
    RefCollision,
    PublicationUncertain,
    RemoteUnavailable,
    PersistenceUncertain,
    Interrupted,
    InvalidAction,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Details {
    pub candidate_id: String,
    pub base: String,
    pub tree: String,
    pub binding: String,
    pub commit: Option<String>,
    pub output_ref: String,
    pub destination: Destination,
    pub source_project: String,
    pub project_suffix: String,
    pub delivery_generation: Option<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Outcome {
    pub status: Status,
    pub operation_id: String,
    pub details: Option<Details>,
    pub local: LocalState,
    pub local_evidence: RefEvidence,
    pub remote: DeliveryState,
    pub remote_evidence: Option<RefEvidence>,
    pub problem: Option<Problem>,
    /// #747 discharges private-resource obligations before delivery is enabled.
    pub cleanup_pending: bool,
}

impl Outcome {
    pub(super) fn unavailable(id: &str, problem: Problem) -> Self {
        Self {
            status: Status::RecoveryRequired,
            operation_id: id.into(),
            details: None,
            local: LocalState::Unavailable,
            local_evidence: RefEvidence::Unreadable,
            remote: DeliveryState::Unavailable,
            remote_evidence: None,
            problem: Some(problem),
            cleanup_pending: true,
        }
    }
}

pub(super) fn classify(
    record: &Record,
    local: RefEvidence,
    remote: Option<RefEvidence>,
    problem: Option<Problem>,
) -> Outcome {
    let d = &record.description;
    let commit = record.state.commit();
    let local_matches =
        matches!(&local, RefEvidence::Direct(value) if Some(value.as_str()) == commit);
    let (local_state, mut status) = match &record.state {
        Phase::Preparing => (LocalState::NotAttempted, Status::PreparationUnknown),
        Phase::Prepared { .. } if local == RefEvidence::Absent => {
            (LocalState::NotAttempted, Status::Prepared)
        }
        Phase::Prepared { .. } if local == RefEvidence::Unreadable => {
            (LocalState::Unavailable, Status::RecoveryRequired)
        }
        Phase::Prepared { .. } => (LocalState::Changed, Status::RecoveryRequired),
        Phase::LocalAttempt { .. } | Phase::LocalPublished { .. } if local_matches => {
            (LocalState::Present, Status::Published)
        }
        Phase::LocalAttempt { .. } if local == RefEvidence::Absent => {
            (LocalState::Unknown, Status::PublicationUnknown)
        }
        Phase::LocalAttempt { .. } | Phase::LocalPublished { .. }
            if local == RefEvidence::Unreadable =>
        {
            (LocalState::Unavailable, Status::RecoveryRequired)
        }
        Phase::LocalAttempt { .. } | Phase::LocalPublished { .. } => {
            (LocalState::Changed, Status::RecoveryRequired)
        }
    };
    let remote_phase = match &record.state {
        Phase::LocalPublished { remote, .. } => Some(remote),
        Phase::Preparing | Phase::Prepared { .. } | Phase::LocalAttempt { .. } => None,
    };
    let delivery = match (remote_phase, &remote) {
        (None | Some(RemotePhase::Unattempted), _) => DeliveryState::NotAttempted,
        (_, Some(RefEvidence::Direct(value))) if Some(value.as_str()) == commit => {
            DeliveryState::Delivered
        }
        (_, Some(RefEvidence::Unreadable) | None) => DeliveryState::Unavailable,
        (Some(RemotePhase::Attempted { .. }), Some(RefEvidence::Absent)) => DeliveryState::Unknown,
        _ => DeliveryState::Changed,
    };
    if local_state == LocalState::Present && delivery == DeliveryState::Delivered {
        status = Status::Delivered;
    }
    if matches!(
        problem,
        Some(
            Problem::PersistenceUncertain
                | Problem::ReceiptUnavailable
                | Problem::RepositoryChanged
                | Problem::RepositoryUnavailable
        )
    ) {
        status = Status::RecoveryRequired;
    }
    Outcome {
        status,
        operation_id: d.operation_id.clone(),
        details: Some(Details {
            candidate_id: d.candidate_id.clone(),
            base: d.base.clone(),
            tree: d.tree.clone(),
            binding: d.binding.clone(),
            commit: commit.map(str::to_owned),
            output_ref: d.output_ref.clone(),
            destination: d.destination.clone(),
            source_project: d
                .repository
                .source
                .join(&d.project)
                .to_string_lossy()
                .into_owned(),
            project_suffix: d.project.clone(),
            delivery_generation: remote_phase
                .and_then(RemotePhase::generation)
                .map(str::to_owned),
        }),
        local: local_state,
        local_evidence: local,
        remote: delivery,
        remote_evidence: remote,
        problem,
        cleanup_pending: true,
    }
}
