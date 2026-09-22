//! Publication only advances durable authorization. Recovery never replays it.

use std::fs::{self, DirBuilder};
use std::os::unix::fs::DirBuilderExt;
use std::path::Path;
use std::time::Duration;

use super::{
    Candidate, Candidates, Error, Result, capture, destination,
    git::{Git, text},
    random_id,
    record::{Description, Phase, Record, Records, RemotePhase, RepositoryIdentity, identity, oid},
    recover::{self, Outcome, Problem, Status},
    refs::{self, RefEvidence},
    transport,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DurableStage {
    Preparing,
    Prepared,
    LocalAttempt,
    LocalPublished,
    RemoteAttempt,
    RemoteDelivered,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PublicationBoundary {
    BeforePersist(DurableStage),
    AfterPersist(DurableStage),
    BeforeCommit,
    CommitCreated,
    RefPrepared,
    RefInstalled,
    BeforePush,
    PushFinished,
    BeforeReconcile,
}

type Observer<'a> = &'a dyn Fn(PublicationBoundary) -> bool;

fn stage(record: &Record) -> DurableStage {
    match &record.state {
        Phase::Preparing => DurableStage::Preparing,
        Phase::Prepared { .. } => DurableStage::Prepared,
        Phase::LocalAttempt { .. } => DurableStage::LocalAttempt,
        Phase::LocalPublished {
            remote: RemotePhase::Unattempted,
            ..
        } => DurableStage::LocalPublished,
        Phase::LocalPublished {
            remote: RemotePhase::Attempted { .. },
            ..
        } => DurableStage::RemoteAttempt,
        Phase::LocalPublished {
            remote: RemotePhase::Delivered { .. },
            ..
        } => DurableStage::RemoteDelivered,
    }
}

/// Repository-scoped publication service. The receipt lock serializes its own
/// writers, while Git owns the independent output-ref transaction lock.
pub struct Publications {
    git: Git,
    repository: RepositoryIdentity,
    records: Records,
}

impl Publications {
    pub fn open(repository: &Path, deadline: Duration) -> Result<Self> {
        let mut git = Git {
            root: repository.to_path_buf(),
            hooks: "/dev/null".into(),
            deadline,
        };
        let identity = RepositoryIdentity::capture(&git)?;
        git.root = identity.source.clone();
        let records = Records::open(&identity.common)?;
        git.hooks = records.root.join("no-hooks");
        match DirBuilder::new().mode(0o700).create(&git.hooks) {
            Ok(()) => (),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => (),
            Err(_) => return Err(Error::new("Cannot establish disabled compose hooks")),
        }
        if !fs::symlink_metadata(&git.hooks).is_ok_and(|m| m.is_dir()) {
            return Err(Error::new("Invalid compose hooks directory"));
        }
        Ok(Self {
            git,
            repository: identity,
            records,
        })
    }

    fn source_binding(&self, description: &Description) -> std::result::Result<(), Problem> {
        if description.repository != self.repository {
            return Err(Problem::RepositoryChanged);
        }
        let current =
            RepositoryIdentity::capture(&self.git).map_err(|_| Problem::RepositoryUnavailable)?;
        if current != self.repository {
            return Err(Problem::RepositoryChanged);
        }
        Ok(())
    }

    fn binding(&self, description: &Description) -> std::result::Result<(), Problem> {
        self.source_binding(description)?;
        let current = destination::resolve(&self.git, &description.remote)
            .map_err(|_| Problem::RemoteUnavailable)?;
        if current != description.destination {
            return Err(Problem::DestinationChanged);
        }
        Ok(())
    }

    fn signing(&self, description: &Description) -> std::result::Result<(), Problem> {
        let current = capture::signing(&self.git).map_err(|_| Problem::SigningUnavailable)?;
        if current != description.signing {
            return Err(Problem::SigningChanged);
        }
        Ok(())
    }

    fn remote_base(&self, d: &Description) -> std::result::Result<(), Problem> {
        self.binding(d)?;
        match transport::observe(&self.git, d, &d.remote_base_ref) {
            RefEvidence::Direct(base) if base == d.base => Ok(()),
            RefEvidence::Unreadable => Err(Problem::RemoteUnavailable),
            RefEvidence::Absent | RefEvidence::Direct(_) | RefEvidence::Symbolic => {
                Err(Problem::RemoteBaseChanged)
            }
        }
    }

    fn persist(&self, record: &Record, observer: Observer<'_>) -> std::result::Result<(), Problem> {
        let stage = stage(record);
        if !observer(PublicationBoundary::BeforePersist(stage)) {
            return Err(Problem::PersistenceUncertain);
        }
        self.records
            .save(record)
            .map_err(|_| Problem::PersistenceUncertain)?;
        if !observer(PublicationBoundary::AfterPersist(stage)) {
            return Err(Problem::PersistenceUncertain);
        }
        Ok(())
    }

    fn refused(&self, description: Description, problem: Problem) -> Outcome {
        let local = refs::observe(&self.git, &description.output_ref);
        let mut result = recover::classify(&Record::new(description), local, None, Some(problem));
        result.status = Status::Refused;
        result.cleanup_pending = false;
        result
    }

    pub fn confirm(&mut self, candidate: &Candidate) -> Outcome {
        self.confirm_observed(candidate, &|_| true)
    }

    pub fn confirm_observed(&mut self, candidate: &Candidate, observer: Observer<'_>) -> Outcome {
        let description = Description::candidate(candidate);
        let id = description.operation_id.clone();
        match self.records.load(&id) {
            Ok(Some(record)) if record.description == description => {
                return self.reconcile(record, None, observer);
            }
            Ok(Some(_)) | Err(_) => return Outcome::unavailable(&id, Problem::ReceiptUnavailable),
            Ok(None) => (),
        }
        let admission = (|| {
            self.remote_base(&description)?;
            self.signing(&description)?;
            if refs::observe(&self.git, &description.output_ref) != RefEvidence::Absent {
                return Err(Problem::RefCollision);
            }
            self.import(candidate)
                .map_err(|_| Problem::ObjectImportFailed)
        })();
        if let Err(problem) = admission {
            return self.refused(description, problem);
        }
        // Definite prerequisite failures precede the durable commit intent.
        // Once that intent exists, a restart cannot prove whether Git ran.
        let ready = (|| {
            if !observer(PublicationBoundary::BeforeCommit) {
                return Err(Problem::Interrupted);
            }
            self.binding(&description)?;
            self.signing(&description)
        })();
        if let Err(problem) = ready {
            return self.refused(description, problem);
        }
        let mut record = Record::new(description);
        if let Err(problem) = self.persist(&record, observer) {
            // Only this call site proves commit-tree was never invoked. A
            // missing receipt after a later attempt must remain uncertain.
            if matches!(self.records.load(&id), Ok(None)) {
                let mut result = self.refused(record.description, problem);
                result.cleanup_pending = true;
                return result;
            }
            return self.finish(record, Some(problem), observer);
        }
        let result = (|| {
            let d = &record.description;
            let mut args = vec!["commit-tree", d.tree.as_str(), "-p", d.base.as_str()];
            args.push(if d.signing.required {
                "-S"
            } else {
                "--no-gpg-sign"
            });
            let output = self
                .git
                .output(&args, candidate.request.message.as_bytes(), None)
                .map_err(|_| Problem::CommitUnavailable)?;
            if !output.status.success() {
                return Err(Problem::CommitUnavailable);
            }
            if !observer(PublicationBoundary::CommitCreated) {
                return Err(Problem::Interrupted);
            }
            let commit = text(output.stdout).map_err(|_| Problem::CommitUnavailable)?;
            if !oid(&commit) || commit.len() != d.base.len() {
                return Err(Problem::CommitUnavailable);
            }
            record.state = Phase::Prepared { commit };
            self.persist(&record, observer)?;
            self.publish_local(&mut record, observer)?;
            self.publish_remote(&mut record, observer)
        })();
        self.finish(record, result.err(), observer)
    }

    fn import(&self, candidate: &Candidate) -> Result<()> {
        let private = Git {
            root: candidate.snapshot_repository(),
            hooks: self.git.hooks.clone(),
            deadline: self.git.deadline,
        };
        let base_tree = self
            .git
            .line(&["rev-parse", &format!("{}^{{tree}}", candidate.preview.base)])?;
        // Tree exclusions, not commit exclusions: measured Git 2.43 otherwise
        // repacks unchanged base blobs for an uncommitted candidate tree.
        let input = format!("{}\n^{base_tree}\n", candidate.preview.tree);
        let pack = private.bytes(
            &["pack-objects", "--stdout", "--revs"],
            input.as_bytes(),
            None,
        )?;
        self.git.bytes(&["index-pack", "--stdin"], &pack, None)?;
        Ok(())
    }

    fn publish_local(
        &self,
        record: &mut Record,
        observer: Observer<'_>,
    ) -> std::result::Result<(), Problem> {
        let Phase::Prepared { commit } = &record.state else {
            return Err(Problem::InvalidAction);
        };
        let commit = commit.clone();
        self.remote_base(&record.description)?;
        let prepared = refs::Prepared::create(&self.git, &record.description.output_ref, &commit)
            .map_err(|_| Problem::RefCollision)?;
        if !observer(PublicationBoundary::RefPrepared) {
            prepared
                .abort()
                .map_err(|_| Problem::PublicationUncertain)?;
            return Err(Problem::Interrupted);
        }
        record.state = Phase::LocalAttempt {
            commit: commit.clone(),
        };
        if let Err(problem) = self.persist(record, observer) {
            prepared
                .abort()
                .map_err(|_| Problem::PublicationUncertain)?;
            return Err(problem);
        }
        prepared
            .commit()
            .map_err(|_| Problem::PublicationUncertain)?;
        if !observer(PublicationBoundary::RefInstalled) {
            return Err(Problem::Interrupted);
        }
        record.state = Phase::LocalPublished {
            commit,
            remote: RemotePhase::Unattempted,
        };
        self.persist(record, observer)
    }

    fn publish_remote(
        &self,
        record: &mut Record,
        observer: Observer<'_>,
    ) -> std::result::Result<(), Problem> {
        let Phase::LocalPublished {
            commit,
            remote: RemotePhase::Unattempted,
        } = &record.state
        else {
            return Err(Problem::InvalidAction);
        };
        let commit = commit.clone();
        self.remote_base(&record.description)?;
        if refs::observe(&self.git, &record.description.output_ref)
            != RefEvidence::Direct(commit.clone())
        {
            return Err(Problem::PublicationUncertain);
        }
        match transport::observe(
            &self.git,
            &record.description,
            &record.description.output_ref,
        ) {
            RefEvidence::Absent => (),
            RefEvidence::Direct(value) if value == commit => {
                record.state = Phase::LocalPublished {
                    commit,
                    remote: RemotePhase::Delivered {
                        generation: random_id().map_err(|_| Problem::PersistenceUncertain)?,
                    },
                };
                return self.persist(record, observer);
            }
            RefEvidence::Unreadable => return Err(Problem::RemoteUnavailable),
            RefEvidence::Direct(_) | RefEvidence::Symbolic => return Err(Problem::RefCollision),
        }
        record.state = Phase::LocalPublished {
            commit,
            remote: RemotePhase::Attempted {
                generation: random_id().map_err(|_| Problem::PersistenceUncertain)?,
            },
        };
        self.persist(record, observer)?;
        self.authorized_push(record, observer)
    }

    fn authorized_push(
        &self,
        record: &mut Record,
        observer: Observer<'_>,
    ) -> std::result::Result<(), Problem> {
        let Phase::LocalPublished {
            commit,
            remote: RemotePhase::Attempted { generation },
        } = &record.state
        else {
            return Err(Problem::InvalidAction);
        };
        let (commit, generation) = (commit.clone(), generation.clone());
        if !observer(PublicationBoundary::BeforePush) {
            return Err(Problem::Interrupted);
        }
        transport::push(&self.git, &record.description, &commit)
            .map_err(|_| Problem::PublicationUncertain)?;
        if !observer(PublicationBoundary::PushFinished) {
            return Err(Problem::Interrupted);
        }
        record.state = Phase::LocalPublished {
            commit,
            remote: RemotePhase::Delivered { generation },
        };
        self.persist(record, observer)
    }

    fn finish(
        &self,
        fallback: Record,
        problem: Option<Problem>,
        observer: Observer<'_>,
    ) -> Outcome {
        match self.records.load(&fallback.description.operation_id) {
            Ok(Some(record)) => self.reconcile(record, problem, observer),
            _ => recover::classify(
                &fallback,
                refs::observe(&self.git, &fallback.description.output_ref),
                None,
                Some(Problem::ReceiptUnavailable),
            ),
        }
    }

    fn reconcile(
        &self,
        mut record: Record,
        mut problem: Option<Problem>,
        observer: Observer<'_>,
    ) -> Outcome {
        if !observer(PublicationBoundary::BeforeReconcile) {
            return recover::classify(
                &record,
                RefEvidence::Unreadable,
                None,
                Some(Problem::Interrupted),
            );
        }
        if let Err(error) = self.source_binding(&record.description) {
            return recover::classify(&record, RefEvidence::Unreadable, None, Some(error));
        }
        let local = refs::observe(&self.git, &record.description.output_ref);
        if let Phase::LocalAttempt { commit } = &record.state
            && local == RefEvidence::Direct(commit.clone())
        {
            record.state = Phase::LocalPublished {
                commit: commit.clone(),
                remote: RemotePhase::Unattempted,
            };
            if let Err(error) = self.persist(&record, observer) {
                problem = Some(error);
            }
        }
        let remote = if let Phase::LocalPublished {
            remote: RemotePhase::Attempted { .. } | RemotePhase::Delivered { .. },
            ..
        } = &record.state
        {
            if let Err(error) = self.binding(&record.description) {
                problem = Some(error);
                Some(RefEvidence::Unreadable)
            } else {
                Some(transport::observe(
                    &self.git,
                    &record.description,
                    &record.description.output_ref,
                ))
            }
        } else {
            None
        };
        if let Phase::LocalPublished {
            commit,
            remote: RemotePhase::Attempted { generation },
        } = &record.state
            && remote == Some(RefEvidence::Direct(commit.clone()))
        {
            record.state = Phase::LocalPublished {
                commit: commit.clone(),
                remote: RemotePhase::Delivered {
                    generation: generation.clone(),
                },
            };
            if let Err(error) = self.persist(&record, observer) {
                problem = Some(error);
            }
        }
        recover::classify(&record, local, remote, problem)
    }

    /// Read and reconcile only. In particular, absence after authorization is
    /// never permission to recreate a branch or regenerate a commit.
    pub fn recover(&mut self, id: &str) -> Outcome {
        self.recover_observed(id, &|_| true)
    }
    pub fn recover_observed(&mut self, id: &str, observer: Observer<'_>) -> Outcome {
        match self.records.load(id) {
            Ok(Some(record)) => self.reconcile(record, None, observer),
            _ => Outcome::unavailable(id, Problem::ReceiptUnavailable),
        }
    }

    pub fn list(&mut self) -> Result<Vec<Outcome>> {
        Ok(self
            .records
            .list()?
            .into_iter()
            .map(|r| self.reconcile(r, None, &|_| true))
            .collect())
    }

    /// Only work that has no prior durable attempt may advance on ordinary
    /// retry. An attempted operation is always reconciled without replay.
    pub fn retry(&mut self, id: &str) -> Outcome {
        self.retry_observed(id, &|_| true)
    }
    pub fn retry_observed(&mut self, id: &str, observer: Observer<'_>) -> Outcome {
        let mut record = match self.records.load(id) {
            Ok(Some(r)) => r,
            _ => return Outcome::unavailable(id, Problem::ReceiptUnavailable),
        };
        let result = match &record.state {
            Phase::Prepared { .. } => self
                .publish_local(&mut record, observer)
                .and_then(|()| self.publish_remote(&mut record, observer)),
            Phase::LocalPublished {
                remote: RemotePhase::Unattempted,
                ..
            } => self.publish_remote(&mut record, observer),
            Phase::Preparing | Phase::LocalAttempt { .. } | Phase::LocalPublished { .. } => {
                return self.reconcile(record, None, observer);
            }
        };
        self.finish(record, result.err(), observer)
    }

    pub fn republish(&mut self, id: &str, generation: &str) -> Outcome {
        self.republish_observed(id, generation, &|_| true)
    }
    pub fn republish_observed(
        &mut self,
        id: &str,
        generation: &str,
        observer: Observer<'_>,
    ) -> Outcome {
        let mut record = match self.records.load(id) {
            Ok(Some(r)) => r,
            _ => return Outcome::unavailable(id, Problem::ReceiptUnavailable),
        };
        let Phase::LocalPublished { commit, remote } = &record.state else {
            return self.reconcile(record, Some(Problem::InvalidAction), observer);
        };
        if !identity(generation) || remote.generation() != Some(generation) {
            return self.reconcile(record, None, observer);
        }
        let commit = commit.clone();
        let result = (|| {
            // Consume even a no-op request before observing the remote. A
            // replay after later deletion is not new informed authorization.
            let next = random_id().map_err(|_| Problem::PersistenceUncertain)?;
            record.state = Phase::LocalPublished {
                commit: commit.clone(),
                remote: RemotePhase::Attempted {
                    generation: next.clone(),
                },
            };
            self.persist(&record, observer)?;
            self.remote_base(&record.description)?;
            if refs::observe(&self.git, &record.description.output_ref)
                != RefEvidence::Direct(commit.clone())
            {
                return Err(Problem::PublicationUncertain);
            }
            match transport::observe(
                &self.git,
                &record.description,
                &record.description.output_ref,
            ) {
                RefEvidence::Absent => self.authorized_push(&mut record, observer),
                RefEvidence::Direct(value) if value == commit => {
                    record.state = Phase::LocalPublished {
                        commit,
                        remote: RemotePhase::Delivered { generation: next },
                    };
                    self.persist(&record, observer)
                }
                RefEvidence::Unreadable => Err(Problem::RemoteUnavailable),
                RefEvidence::Direct(_) | RefEvidence::Symbolic => Err(Problem::RefCollision),
            }
        })();
        self.finish(record, result.err(), observer)
    }

    pub fn start_alternative(&mut self, candidates: &mut Candidates, id: &str) -> Result<()> {
        let record = self
            .records
            .load(id)?
            .ok_or_else(|| Error::new("The prior compose result is unavailable"))?;
        if !matches!(record.state, Phase::LocalPublished { .. }) {
            return Err(Error::new(
                "Resolve the prior publication before starting an alternative",
            ));
        }
        if record.description.repository != self.repository
            || RepositoryIdentity::capture(&self.git)? != self.repository
            || fs::canonicalize(&candidates.config.project).ok().as_ref()
                != Some(&self.repository.source.join(&record.description.project))
            || self.git.line(&["rev-parse", "--verify", "HEAD^{commit}"])?
                != record.description.base
        {
            return Err(Error::new(
                "The source base changed; start a separately reviewed workflow",
            ));
        }
        match &candidates.current {
            Some(super::Stored::Confirmed(candidate)) if candidate.preview.operation_id == id => (),
            None => (),
            _ => {
                return Err(Error::new(
                    "This browser workflow belongs to another operation",
                ));
            }
        }
        candidates.current = None;
        candidates.alternative_base = Some(record.description.base);
        Ok(())
    }
}
