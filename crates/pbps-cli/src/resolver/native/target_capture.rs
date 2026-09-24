//! Target capture is bound to one owned native connection and executable
//! observations. The first catalog read supplies only file-resolution hints;
//! the returned fresh read is enclosed by the native build observations.

use super::{BoundTarget, NativeTarget, correlate, engine, executables};
use pbps_db::resolver::{
    InstanceObservation,
    capture::{CaptureDifference, InputChange},
    environment::{ExecutableIdentity, ExecutableRole, ExecutableSet, Provenance},
};
use pbps_pg::resolver::capture::{CaptureError, CaptureScope, CapturedInputs};

#[derive(Debug, thiserror::Error)]
pub enum CaptureFailure {
    #[error("target capture requires a qualified PostgreSQL native connection")]
    Binding,
    #[error(transparent)]
    Catalog(#[from] CaptureError),
    #[error("required target executable content is unreadable or unqualified")]
    Executables,
    #[error("target inputs or executable content changed during capture")]
    Changed,
}

/// No ordinary serialization or Debug: the catalog holds source and the
/// executable/content comparisons are input verifiers, not public evidence.
pub struct CapturedTargetInputs {
    catalog: CapturedInputs,
    executables: ExecutableSet,
    instance: InstanceObservation,
}

impl CapturedTargetInputs {
    pub fn catalog(&self) -> &CapturedInputs {
        &self.catalog
    }

    pub fn compare(&self, current: &Self) -> Vec<CaptureDifference> {
        let mut differences = self.catalog.compare(&current.catalog);
        if self.instance != current.instance || self.executables != current.executables {
            differences.push(CaptureDifference {
                object: None,
                change: InputChange::Environment,
            });
        }
        differences
    }
}

async fn check(bound: &mut BoundTarget) -> Result<(), CaptureFailure> {
    bound.check_native().map_err(|_| CaptureFailure::Binding)?;
    let identity = engine::identity(&mut bound.connection)
        .await
        .map_err(|_| CaptureFailure::Binding)?;
    if identity != bound.identity {
        return Err(CaptureFailure::Binding);
    }
    correlate(&bound.lease, &identity).map_err(|_| CaptureFailure::Binding)?;
    bound
        .lease
        .check(&bound.connection)
        .map_err(|_| CaptureFailure::Binding)
}

fn qualified(executables: &ExecutableSet) -> bool {
    fn readable(input: &ExecutableIdentity) -> bool {
        let Some(digest) = &input.digest else {
            return false;
        };
        if digest.len() != 64
            || !digest
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return false;
        }
        matches!(
            (&input.role, &input.provenance),
            (
                ExecutableRole::Engine | ExecutableRole::Preloaded,
                Provenance::LoadedContent
            ) | (ExecutableRole::LateLoaded, Provenance::DiskCandidate)
        )
    }
    executables.engine.role == ExecutableRole::Engine
        && readable(&executables.engine)
        && executables.libraries.iter().all(readable)
}

impl NativeTarget {
    pub async fn capture_postgres(
        &mut self,
        scope: &CaptureScope,
    ) -> Result<CapturedTargetInputs, CaptureFailure> {
        // Ownership moves before the first await. Cancellation, even during
        // an owned SQL transaction, drops the connection and every weak lease.
        let mut bound = self.current.take().ok_or(CaptureFailure::Binding)?;
        if bound.connection.driver() != pbps_db::Driver::Postgres {
            return Err(CaptureFailure::Binding);
        }
        check(&mut bound).await?;
        let hints = pbps_pg::resolver::capture::capture(&mut bound.connection, scope).await?;
        let required = hints.runtime_inputs().map_err(CaptureError::Coverage)?;
        let before = executables::executables(
            bound.lease.owner(),
            &required.libraries,
            &required.dynamic_library_path,
            &[],
        )
        .await
        .map_err(|_| CaptureFailure::Executables)?;
        if !qualified(&before) {
            return Err(CaptureFailure::Executables);
        }
        check(&mut bound).await?;
        let catalog = pbps_pg::resolver::capture::capture(&mut bound.connection, scope).await?;
        if !hints.compare(&catalog).is_empty() {
            return Err(CaptureFailure::Changed);
        }
        let after = executables::executables(
            bound.lease.owner(),
            &required.libraries,
            &required.dynamic_library_path,
            &[],
        )
        .await
        .map_err(|_| CaptureFailure::Executables)?;
        if !qualified(&after) {
            return Err(CaptureFailure::Executables);
        }
        if before != after {
            return Err(CaptureFailure::Changed);
        }
        check(&mut bound).await?;
        let captured = CapturedTargetInputs {
            catalog,
            executables: after,
            instance: bound.identity.clone(),
        };
        self.current = Some(bound);
        Ok(captured)
    }

    pub async fn recapture_postgres(
        &mut self,
        previous: &CapturedTargetInputs,
    ) -> Result<(CapturedTargetInputs, Vec<CaptureDifference>), CaptureFailure> {
        let current = self.capture_postgres(previous.catalog.scope()).await?;
        let differences = previous.compare(&current);
        Ok((current, differences))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(role: ExecutableRole, provenance: Provenance) -> ExecutableIdentity {
        ExecutableIdentity {
            role,
            path: "fixture".into(),
            digest: Some("a".repeat(64)),
            provenance,
            disk_differs_from_loaded: Some(false),
        }
    }

    #[test]
    fn a_disk_copy_cannot_certify_a_running_engine_or_library() {
        let mut inputs = ExecutableSet {
            engine: fixture(ExecutableRole::Engine, Provenance::LoadedContent),
            libraries: vec![fixture(
                ExecutableRole::Preloaded,
                Provenance::LoadedContent,
            )],
        };
        assert!(qualified(&inputs));
        inputs.engine.provenance = Provenance::DiskCandidate;
        assert!(!qualified(&inputs));
        inputs.engine.provenance = Provenance::LoadedContent;
        inputs.libraries[0].provenance = Provenance::DiskCandidate;
        assert!(!qualified(&inputs));
        inputs.libraries[0].role = ExecutableRole::LateLoaded;
        assert!(qualified(&inputs));
        inputs.libraries[0].digest = None;
        assert!(!qualified(&inputs));
    }
}
