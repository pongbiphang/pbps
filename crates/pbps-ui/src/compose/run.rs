//! Publication only advances durable authorization. Recovery never replays it.

use std::fs;
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
    CommitKnown,
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
        Phase::CommitKnown { .. } => DurableStage::CommitKnown,
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
    resources: super::resources::Resources,
}

impl Publications {
    pub fn open(repository: &Path, deadline: Duration) -> Result<Self> {
        Self::open_with_resources(repository, deadline, super::ResourceObserver::default())
    }

    pub fn open_with_resources(
        repository: &Path,
        deadline: Duration,
        observer: super::ResourceObserver,
    ) -> Result<Self> {
        let mut git = Git {
            root: repository.to_path_buf(),
            hooks: "/dev/null".into(),
            deadline,
        };
        let identity = RepositoryIdentity::capture(&git)?;
        git.root = identity.source.clone();
        super::durable::refuse_legacy(&git)?;
        let records = Records::open(&identity.common, observer.clone())?;
        git.hooks = records.root.join("no-hooks");
        records.directory.child("no-hooks", true)?;
        let resources = super::resources::Resources::open(&git, observer)?;
        Ok(Self {
            git,
            repository: identity,
            records,
            resources,
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
        // Definite publication refusal does not discharge candidate resources.
        // An unreadable report is unresolved; only acknowledged retirement can
        // say that no private cleanup remains, including a spent old handle.
        let cleanup_pending = self
            .resources
            .report(&description.operation_id)
            .map_or(true, |report| report.cleanup_pending);
        let mut result = recover::classify(&Record::new(description), local, None, Some(problem));
        result.status = Status::Refused;
        result.cleanup_pending = cleanup_pending;
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
            self.source_binding(&description)?;
            if self
                .resources
                .preparation_recorded(&id)
                .map_err(|_| Problem::ResourceUnavailable)?
            {
                return Err(Problem::ReceiptUnavailable);
            }
            self.resources
                .admit(&id, &description.binding, &description.base, None)
                .map_err(|_| Problem::ResourceUnavailable)?;
            self.remote_base(&description)?;
            self.signing(&description)?;
            if refs::observe(&self.git, &description.output_ref) != RefEvidence::Absent {
                return Err(Problem::RefCollision);
            }
            self.import(candidate)
                .map_err(|_| Problem::ObjectImportFailed)
        })();
        if let Err(problem) = admission {
            if problem == Problem::ReceiptUnavailable {
                return Outcome::unavailable(&id, problem);
            }
            return self.refused(description, problem);
        }
        // Definite prerequisite failures precede the durable commit intent.
        // Once that intent exists, a restart cannot prove whether Git ran.
        let ready = (|| {
            if !observer(PublicationBoundary::BeforeCommit) {
                return Err(Problem::Interrupted);
            }
            self.binding(&description)?;
            self.signing(&description)?;
            // Ask Git to validate both identities before recording an attempt.
            // Neither identity diagnostics nor a preflight timestamp is evidence
            // to persist; commit-tree still uses the ordinary configured identity.
            for identity in ["GIT_AUTHOR_IDENT", "GIT_COMMITTER_IDENT"] {
                self.git
                    .bytes(&["var", identity], &[], None)
                    .map_err(|_| Problem::IdentityUnavailable)?;
            }
            Ok(())
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
            // The policy check above reads configuration a moment before
            // commit-tree reads it again. Pass the sealed public selectors
            // explicitly so a writer in between cannot substitute a key or
            // format; an unset format is pinned to Git's own default. An
            // implicit key stays implicit, as reviewed (#770).
            let format = format!(
                "gpg.format={}",
                d.signing.format.as_deref().unwrap_or("openpgp")
            );
            let key = format!("-S{}", d.signing.key.as_deref().unwrap_or(""));
            let mut args = vec!["-c", "core.fsync=all", "-c", "core.fsyncMethod=fsync"];
            if d.signing.required {
                args.extend(["-c", format.as_str()]);
            }
            args.extend(["commit-tree", d.tree.as_str(), "-p", d.base.as_str()]);
            args.push(if d.signing.required {
                key.as_str()
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
            record.state = Phase::CommitKnown {
                commit: commit.clone(),
            };
            self.persist(&record, observer)?;
            self.resources
                .known_commit(&id, &commit)
                .map_err(|_| Problem::ResourceUnavailable)?;
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
        self.git.bytes(
            &[
                "-c",
                "core.fsync=all",
                "-c",
                "core.fsyncMethod=fsync",
                "index-pack",
                "--stdin",
            ],
            &pack,
            None,
        )?;
        self.resources.flush_import()?;
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
        self.resources
            .admit(
                &record.description.operation_id,
                &record.description.binding,
                &record.description.base,
                Some(&commit),
            )
            .map_err(|_| Problem::ResourceUnavailable)?;
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
        self.resources
            .flush_publication_ref(&record.description.output_ref)
            .map_err(|_| Problem::PersistenceUncertain)?;
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
        self.resources
            .admit(
                &record.description.operation_id,
                &record.description.binding,
                &record.description.base,
                Some(&commit),
            )
            .map_err(|_| Problem::ResourceUnavailable)?;
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
        let signing = self.push_signing(&record.description, &commit)?;
        record.state = Phase::LocalPublished {
            commit,
            remote: RemotePhase::Attempted {
                generation: random_id().map_err(|_| Problem::PersistenceUncertain)?,
            },
        };
        self.persist(record, observer)?;
        self.authorized_push(record, observer, signing)
    }

    fn push_signing(
        &self,
        description: &Description,
        commit: &str,
    ) -> std::result::Result<transport::PushSigning, Problem> {
        let signing =
            transport::push_signing(&self.git).map_err(|_| Problem::SigningUnavailable)?;
        transport::preflight(&self.git, description, commit, signing).map_err(|refusal| {
            match refusal {
                transport::Preflight::Unsupported => Problem::PushSigningUnsupported,
                transport::Preflight::Unavailable => Problem::RemoteUnavailable,
            }
        })?;
        Ok(signing)
    }

    fn authorized_push(
        &self,
        record: &mut Record,
        observer: Observer<'_>,
        signing: transport::PushSigning,
    ) -> std::result::Result<(), Problem> {
        let Phase::LocalPublished {
            commit,
            remote: RemotePhase::Attempted { generation },
        } = &record.state
        else {
            return Err(Problem::InvalidAction);
        };
        let (commit, generation) = (commit.clone(), generation.clone());
        self.resources
            .admit(
                &record.description.operation_id,
                &record.description.binding,
                &record.description.base,
                Some(&commit),
            )
            .map_err(|_| Problem::ResourceUnavailable)?;
        if !observer(PublicationBoundary::BeforePush) {
            return Err(Problem::Interrupted);
        }
        transport::push(&self.git, &record.description, &commit, signing)
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

    fn resume_preparation(
        &self,
        record: &mut Record,
        observer: Observer<'_>,
    ) -> std::result::Result<(), Problem> {
        let Phase::CommitKnown { commit } = &record.state else {
            return Ok(());
        };
        self.source_binding(&record.description)?;
        if !self
            .resources
            .preparation_ready(
                &record.description.operation_id,
                &record.description.binding,
                &record.description.base,
                commit,
            )
            .map_err(|_| Problem::ResourceUnavailable)?
        {
            return Ok(());
        }
        // Acknowledged ownership closes the pre-pin uncertainty. Complete only
        // the receipt; recovery never invokes commit-tree or publishes a ref.
        let mut prepared = record.clone();
        prepared.state = Phase::Prepared {
            commit: commit.clone(),
        };
        self.persist(&prepared, observer)?;
        *record = prepared;
        Ok(())
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
        if let Err(error) = self.resume_preparation(&mut record, observer) {
            problem = Some(error);
        }
        let local = refs::observe(&self.git, &record.description.output_ref);
        if !matches!(record.state, Phase::Preparing | Phase::CommitKnown { .. })
            && self
                .resources
                .admit(
                    &record.description.operation_id,
                    &record.description.binding,
                    &record.description.base,
                    record.state.commit(),
                )
                .is_err()
        {
            problem = Some(Problem::ResourceUnavailable);
        }
        if let Phase::LocalAttempt { commit } = &record.state
            && local == RefEvidence::Direct(commit.clone())
        {
            if self
                .resources
                .flush_publication_ref(&record.description.output_ref)
                .is_err()
            {
                return recover::classify(
                    &record,
                    local,
                    None,
                    Some(Problem::PersistenceUncertain),
                );
            }
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
        let mut result = recover::classify(&record, local, remote, problem);
        result.cleanup_pending = self
            .resources
            .report(&record.description.operation_id)
            .map_or(true, |r| r.cleanup_pending);
        if matches!(
            problem,
            Some(
                Problem::ResourceUnavailable
                    | Problem::ReceiptUnavailable
                    | Problem::PersistenceUncertain
            )
        ) {
            result.cleanup_pending = true;
        }
        result
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
        let mut outcomes = Vec::new();
        for record in self.records.list()? {
            // A receipt outside every source scope is named, like a bad read.
            let path = self
                .records
                .directory
                .path
                .join(format!("{}.json", record.description.operation_id));
            if record
                .description
                .repository
                .source_scope(&self.repository)
                .map_err(|error| error.at(&path))?
            {
                outcomes.push(self.reconcile(record, None, &|_| true));
            }
        }
        Ok(outcomes)
    }

    pub fn resource_reports(&self) -> Result<Vec<super::ResourceReport>> {
        self.resources.list()
    }

    pub fn recover_resources(&mut self, id: &str) -> Result<super::ResourceReport> {
        self.recover_resources_at(id, std::time::SystemTime::now())
    }

    pub fn recover_resources_at(
        &mut self,
        id: &str,
        now: std::time::SystemTime,
    ) -> Result<super::ResourceReport> {
        if self.records.load(id)?.is_none() {
            self.resources.expire(id, now)?;
        }
        self.resources.resume_retirement(id)
    }

    pub fn discard_preview(&mut self, id: &str) -> Result<super::ResourceReport> {
        if self.records.load(id)?.is_some() {
            return Err(Error::new(
                "A publication receipt cannot be discarded as a preview",
            ));
        }
        self.resources.discard_preview(id)?;
        self.resources.report(id)
    }

    /// Retire a completed snapshot while the exact commit stays rooted for its
    /// retained receipt. An uncertain publication never enters this path.
    pub fn cleanup(&mut self, id: &str) -> Outcome {
        let record = match self.records.load(id) {
            Ok(Some(record)) => record,
            _ => return Outcome::unavailable(id, Problem::ReceiptUnavailable),
        };
        if !matches!(
            record.state,
            Phase::LocalPublished {
                remote: RemotePhase::Delivered { .. },
                ..
            }
        ) {
            return self.reconcile(record, Some(Problem::InvalidAction), &|_| true);
        }
        let problem = self
            .resources
            .retire(id, true)
            .err()
            .map(|_| Problem::ResourceUnavailable);
        self.finish(record, problem, &|_| true)
    }

    /// Explicitly forget a completed receipt; public Git branches are untouched.
    pub fn forget(&mut self, id: &str) -> Result<super::ResourceReport> {
        let record = self.records.load(id)?;
        if let Some(record) = record {
            if !matches!(
                record.state,
                Phase::LocalPublished {
                    remote: RemotePhase::Delivered { .. },
                    ..
                }
            ) {
                return Err(Error::new(
                    "Resolve publication before forgetting its receipt",
                ));
            }
            self.source_binding(&record.description)
                .map_err(|_| Error::new("The receipt repository is unavailable or changed"))?;
            self.resources.retire(id, false)?;
            self.records.directory.remove(&format!("{id}.json"))?;
        }
        let report = self.resources.report(id)?;
        if report.state != super::ResourceState::Spent {
            return Err(Error::new(
                "Receipt retirement is incomplete; preserve its resource evidence",
            ));
        }
        Ok(report)
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
        if let Err(error) = self.resume_preparation(&mut record, observer) {
            return self.finish(record, Some(error), observer);
        }
        let result = match &record.state {
            Phase::Prepared { .. } => self
                .publish_local(&mut record, observer)
                .and_then(|()| self.publish_remote(&mut record, observer)),
            Phase::LocalPublished {
                remote: RemotePhase::Unattempted,
                ..
            } => self.publish_remote(&mut record, observer),
            Phase::Preparing
            | Phase::CommitKnown { .. }
            | Phase::LocalAttempt { .. }
            | Phase::LocalPublished { .. } => {
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
                RefEvidence::Absent => self
                    .push_signing(&record.description, &commit)
                    .and_then(|signing| self.authorized_push(&mut record, observer, signing)),
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
