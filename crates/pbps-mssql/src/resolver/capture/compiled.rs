//! The producer owns both the fresh capture and the key chosen before it.

use super::{CaptureError, CapturedInputs, ObjectIdentity, RecordedOwnership};
use pbps_model::resolver::ObjectOwnership;
use std::collections::BTreeMap;

#[derive(Clone)]
pub struct BindingRecord {
    pub object: ObjectIdentity,
    pub ownership: ObjectOwnership,
    pub bindings: Vec<pbps_model::resolver::Binding>,
}

pub struct CompiledCapture {
    captured: CapturedInputs,
    ownership: BTreeMap<ObjectIdentity, ObjectOwnership>,
}

impl CompiledCapture {
    fn from_capture(
        captured: CapturedInputs,
        recorded: &RecordedOwnership<'_>,
        reconstruction: &crate::resolver::reconstruct::Reconstruction,
    ) -> Result<Self, CaptureError> {
        captured
            .require_runtime_candidates()
            .map_err(CaptureError::Coverage)?;
        let present = captured.objects().collect();
        if !reconstruction.absent_from(&present).is_empty() {
            // A successful CREATE missing from its own read is no binding
            // verdict: the catalog read is incomplete (SPEC 9.8).
            return Err(CaptureError::Incomplete);
        }
        let ownership =
            super::ownership::classify(&captured, recorded).map_err(CaptureError::Coverage)?;
        Ok(Self {
            captured,
            ownership,
        })
    }

    pub fn assess_staged(
        &self,
        target: &CapturedInputs,
        managed: &super::CaptureScope,
        reconstruction: &crate::resolver::reconstruct::Reconstruction,
        staged: &super::Staged,
    ) -> pbps_db::resolver::capture::Assessment {
        super::assess::assess(
            target,
            &self.captured,
            &self.ownership,
            managed,
            reconstruction,
            staged,
        )
    }

    pub fn planning_records(
        &self,
    ) -> Result<Vec<BindingRecord>, pbps_model::resolver::ManifestError> {
        Ok(self
            .captured
            .inputs
            .iter()
            .map(|(object, input)| BindingRecord {
                object: object.clone(),
                ownership: self
                    .ownership
                    .get(object)
                    .cloned()
                    .unwrap_or(ObjectOwnership::Unqualified),
                bindings: input
                    .bindings
                    .iter()
                    .map(|b| pbps_model::resolver::Binding {
                        node: b.node.clone(),
                        path: b.path.clone(),
                        target: b.target.clone(),
                    })
                    .collect(),
            })
            .collect())
    }

    pub fn seal_for_plan(
        self,
    ) -> Result<pbps_model::resolver::InputManifest, pbps_model::resolver::ManifestError> {
        self.captured
            .seal(self.captured.producer.key(), &self.ownership)
    }
}

/// Capture scratch with the key fixed before any identity is constructed.
pub async fn capture_desired_for_plan(
    connection: &mut impl pbps_db::transport::QueryConnection,
    scope: &super::CaptureScope,
    declarations: &pbps_model::dependency::Dependencies,
    key: &pbps_db::fingerprint::EnvironmentFingerprintKey,
    recorded: &RecordedOwnership<'_>,
    reconstruction: &crate::resolver::reconstruct::Reconstruction,
) -> Result<CompiledCapture, super::CaptureError> {
    let captured = super::capture_declared(connection, scope, declarations, key).await?;
    CompiledCapture::from_capture(captured, recorded, reconstruction)
}
