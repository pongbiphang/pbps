//! Target capture is bound to one owned native connection and executable
//! observations. The first catalog read supplies only file-resolution hints;
//! the returned fresh read is enclosed by the native build observations.

use super::{BoundTarget, NativeTarget, correlate, engine, executables};
use engine::{CaptureScope, CapturedInputs, RecordedOwnership};
use pbps_db::fingerprint::EnvironmentFingerprintKey;
use pbps_db::resolver::{
    InstanceObservation,
    capture::{CaptureDifference, CaptureError, InputChange},
    environment::{ExecutableIdentity, ExecutableRole, Provenance},
};
use pbps_model::resolver::InputManifest;

#[derive(Debug, thiserror::Error)]
pub enum CaptureFailure {
    #[error("target capture requires a qualified native connection")]
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
    executables: executables::CapturedExecutables,
    instance: InstanceObservation,
}

impl CapturedTargetInputs {
    pub fn catalog(&self) -> &CapturedInputs {
        &self.catalog
    }

    #[cfg(test)]
    pub(super) fn native_library_count(&self) -> usize {
        self.executables.libraries().count()
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

fn qualified<'a>(
    engine: &ExecutableIdentity,
    libraries: impl Iterator<Item = &'a ExecutableIdentity>,
) -> bool {
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
    engine.role == ExecutableRole::Engine && readable(engine) && libraries.into_iter().all(readable)
}

/// Versioned target-only native observation. Canonical roles, content,
/// provenance, multiplicity and disk-divergence are retained; run-local paths,
/// PIDs, mapping addresses and connection IDs are excluded. A future apply
/// reader can call this after its own native lease and catalog hints read.
fn build_fingerprint(
    key: &EnvironmentFingerprintKey,
    observed: &executables::CapturedExecutables,
) -> Result<String, CaptureFailure> {
    let engine = observed.engine().clone();
    // These are process-namespace loader paths, not inspector host prefixes or
    // PIDs. Retaining a mapped file's path distinguishes a code swap between
    // two loaded libraries; opaque required names are keyed separately below.
    let mut libraries = observed.libraries().cloned().collect::<Vec<_>>();
    libraries.sort_by_cached_key(|library| {
        serde_json::to_vec(library).expect("qualified executable serializes")
    });
    let associations = observed
        .required_associations(key)
        .map_err(|_| CaptureFailure::Executables)?;
    let bytes = serde_json::to_vec(&(engine, libraries, associations))
        .map_err(|_| CaptureFailure::Executables)?;
    Ok(crate::resolver::sealing::hex(key.fingerprint(
        "pbps/pg-native-build/v1",
        "qualified-executables",
        &bytes,
    )))
}

impl NativeTarget {
    /// `dropped` are the dropped routines' signatures to identify in the
    /// capture's own snapshot, which both reads below carry (#1148).
    pub async fn capture_postgres(
        &mut self,
        scope: &CaptureScope,
        dropped: &std::collections::BTreeSet<engine::DroppedSignature>,
    ) -> Result<CapturedTargetInputs, CaptureFailure> {
        self.capture_postgres_inner(scope, dropped, None, None)
            .await
            .map(|(captured, _, _)| captured)
    }

    /// The key is chosen before the fresh catalog read. Neither the returned
    /// native capture nor the keyed manifest can be sealed under another key.
    pub async fn capture_postgres_sealed(
        &mut self,
        scope: &CaptureScope,
        dropped: &std::collections::BTreeSet<engine::DroppedSignature>,
        key: &EnvironmentFingerprintKey,
    ) -> Result<(CapturedTargetInputs, InputManifest), CaptureFailure> {
        let (captured, manifest, _) = self
            .capture_postgres_inner(scope, dropped, Some(key), None)
            .await?;
        Ok((
            captured,
            manifest.expect("sealed capture returns a manifest"),
        ))
    }

    /// This is the evidence producer's controlled fresh read. Recorded
    /// identities enter before catalog kind facts are erased by sealing;
    /// native build content is pinned inside the same executable bracket.
    pub async fn capture_postgres_qualified(
        &mut self,
        scope: &CaptureScope,
        dropped: &std::collections::BTreeSet<engine::DroppedSignature>,
        key: &EnvironmentFingerprintKey,
        recorded: &RecordedOwnership<'_>,
    ) -> Result<(CapturedTargetInputs, InputManifest, String), CaptureFailure> {
        let (captured, manifest, build) = self
            .capture_postgres_inner(scope, dropped, Some(key), Some(recorded))
            .await?;
        Ok((
            captured,
            manifest.expect("qualified capture returns a manifest"),
            build.expect("qualified capture returns a build fingerprint"),
        ))
    }

    async fn capture_postgres_inner(
        &mut self,
        scope: &CaptureScope,
        dropped: &std::collections::BTreeSet<engine::DroppedSignature>,
        key: Option<&EnvironmentFingerprintKey>,
        recorded: Option<&RecordedOwnership<'_>>,
    ) -> Result<(CapturedTargetInputs, Option<InputManifest>, Option<String>), CaptureFailure> {
        // Ownership moves before the first await. Cancellation, even during
        // an owned SQL transaction, drops the connection and every weak lease.
        let mut bound = self.current.take().ok_or(CaptureFailure::Binding)?;
        check(&mut bound).await?;
        // Native-input authority comes only from our own fresh connected read.
        // The returned catalog alone cannot recreate it (DEC-974.1).
        let (hints, required) =
            engine::capture_with_runtime_inputs(&mut bound.connection, scope, dropped).await?;
        let before = executables::captured_executables(bound.lease.owner(), &required)
            .await
            .map_err(|_| CaptureFailure::Executables)?;
        if !qualified(before.engine(), before.libraries()) {
            return Err(CaptureFailure::Executables);
        }
        check(&mut bound).await?;
        let (catalog, manifest) = match key {
            Some(key) => {
                let (catalog, manifest) = match recorded {
                    Some(recorded) => {
                        engine::capture_qualified(
                            &mut bound.connection,
                            scope,
                            dropped,
                            key,
                            recorded,
                        )
                        .await?
                    }
                    None => {
                        engine::capture_sealed(&mut bound.connection, scope, dropped, key).await?
                    }
                };
                (catalog, Some(manifest))
            }
            None => (
                engine::capture(&mut bound.connection, scope, dropped).await?,
                None,
            ),
        };
        if !hints.compare(&catalog).is_empty() {
            return Err(CaptureFailure::Changed);
        }
        let after = executables::captured_executables(bound.lease.owner(), &required)
            .await
            .map_err(|_| CaptureFailure::Executables)?;
        if !qualified(after.engine(), after.libraries()) {
            return Err(CaptureFailure::Executables);
        }
        if before != after {
            return Err(CaptureFailure::Changed);
        }
        check(&mut bound).await?;
        let build = if let (Some(key), Some(_)) = (key, recorded) {
            Some(build_fingerprint(key, &after)?)
        } else {
            None
        };
        let captured = CapturedTargetInputs {
            catalog,
            executables: after,
            instance: bound.identity.clone(),
        };
        self.current = Some(bound);
        Ok((captured, manifest, build))
    }

    pub async fn recapture_postgres(
        &mut self,
        previous: &CapturedTargetInputs,
    ) -> Result<(CapturedTargetInputs, Vec<CaptureDifference>), CaptureFailure> {
        let signatures = previous.catalog.dropped().keys().cloned().collect();
        let current = self
            .capture_postgres(previous.catalog.scope(), &signatures)
            .await?;
        let differences = previous.compare(&current);
        Ok((current, differences))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pbps_db::resolver::environment::ExecutableSet;

    #[test]
    fn capture_lifecycle_routes_without_naming_an_adapter() {
        // This is an architectural requirement: live PG behavior alone cannot
        // distinguish a routed read from the same adapter called directly.
        let source = include_str!("target_capture.rs");
        let (production, _) = source.rsplit_once("\n#[cfg(test)]\nmod tests {").unwrap();
        for adapter in ["pbps_pg", "pbps_mssql"] {
            assert!(
                !production.contains(adapter),
                "lifecycle bypasses engine routing: {adapter}"
            );
        }
    }

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
        assert!(qualified(&inputs.engine, inputs.libraries.iter()));
        inputs.engine.provenance = Provenance::DiskCandidate;
        assert!(!qualified(&inputs.engine, inputs.libraries.iter()));
        inputs.engine.provenance = Provenance::LoadedContent;
        inputs.libraries[0].provenance = Provenance::DiskCandidate;
        assert!(!qualified(&inputs.engine, inputs.libraries.iter()));
        inputs.libraries[0].role = ExecutableRole::LateLoaded;
        assert!(qualified(&inputs.engine, inputs.libraries.iter()));
        inputs.libraries[0].digest = None;
        assert!(!qualified(&inputs.engine, inputs.libraries.iter()));
    }
}
