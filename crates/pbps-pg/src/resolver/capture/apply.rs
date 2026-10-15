//! Apply's captures of a sealed manifest (#616): the manifest's own scope and
//! signature lookups, read again inside the apply's transaction and sealed
//! under the plan's key, for the caller to compare with the seal.
//!
//! Nothing here decides whether the target still matches; the comparison is
//! the model's, over two manifests sealed by the same rule under the same key.

use super::{CandidateClass, CandidateSet, CaptureError, CaptureScope, DroppedSignature};
use super::{RecordedOwnership, manifest, read, scope};
use pbps_db::resolver::capture::ObjectIdentity;
use pbps_model::resolver::{InputManifest, ObjectOwnership};
use std::collections::{BTreeMap, BTreeSet};

/// The opening recheck, before apply's first statement: `sealed`'s scope and
/// lookups as the target holds them now.
///
/// Each record keeps the ownership the seal proved for it. Ownership follows
/// from the recorded declarations and the record's own catalog facts: apply
/// has already refused a ledger other than the plan's baseline, and a
/// record whose facts moved differs in its properties, so a fresh
/// classification could only repeat the sealed one or refuse what the
/// properties refuse anyway.
pub async fn recheck_opening(
    connection: &mut impl pbps_db::transport::QueryConnection,
    sealed: &InputManifest,
    key: &pbps_db::fingerprint::EnvironmentFingerprintKey,
) -> Result<InputManifest, CaptureError> {
    let (scope, dropped) = request(sealed, &BTreeSet::new())?;
    let captured = capture(connection, &scope, &dropped).await?;
    let ownership = sealed_ownership(sealed);
    captured
        .seal_with_roles(key, None, Some(&ownership))
        .map_err(|_| CaptureError::Incomplete)
}

/// The closing capture, after apply's last statement: `sealed`'s scope and
/// lookups as the plan left the target, and the records it holds that the
/// seal does not, each with the ownership `recorded`, the plan's
/// declarations, proves for it (DEC-614.2).
///
/// A record the plan installed with a name the engine chose can have
/// another name here than on scratch, so `vanishable` are the sealed records
/// the plan installs whose names the read must not insist on: left out of
/// the retained roots, they are read where they are found and not refused
/// where they are not (#616).
pub async fn capture_closing(
    connection: &mut impl pbps_db::transport::QueryConnection,
    sealed: &InputManifest,
    vanishable: &BTreeSet<ObjectIdentity>,
    key: &pbps_db::fingerprint::EnvironmentFingerprintKey,
    recorded: &RecordedOwnership<'_>,
) -> Result<InputManifest, CaptureError> {
    let (scope, dropped) = request(sealed, vanishable)?;
    let captured = capture(connection, &scope, &dropped).await?;
    let mut ownership =
        super::ownership::classify(&captured, recorded).map_err(CaptureError::Coverage)?;
    // A record the seal holds is compared under the seal's ownership; the
    // fresh classification speaks only for the records it does not.
    ownership.extend(sealed_ownership(sealed));
    captured
        .seal_with_roles(key, None, Some(&ownership))
        .map_err(|_| CaptureError::Incomplete)
}

async fn capture(
    connection: &mut impl pbps_db::transport::QueryConnection,
    scope_request: &CaptureScope,
    dropped: &BTreeSet<DroppedSignature>,
) -> Result<super::CapturedInputs, CaptureError> {
    let mut prepared = None;
    let read = read::in_callers_transaction(connection, dropped, |catalog, major| {
        let mut result = scope::prepare(catalog, major, scope_request)?;
        let selection = std::mem::take(&mut result.render);
        prepared = Some(result);
        Ok(selection)
    })
    .await?;
    manifest::finish(
        read,
        prepared.ok_or(CaptureError::Incomplete)?,
        scope_request.clone(),
    )
    .map_err(CaptureError::Coverage)
}

/// What `sealed` asked its read for, less `vanishable` among its roots. A
/// class or lookup kind this build does not read refuses: it would be read
/// as nothing.
fn request(
    sealed: &InputManifest,
    vanishable: &BTreeSet<ObjectIdentity>,
) -> Result<(CaptureScope, BTreeSet<DroppedSignature>), CaptureError> {
    let candidates = sealed
        .scope()
        .candidates
        .iter()
        .map(|set| {
            Ok(CandidateSet {
                class: CandidateClass::of_catalog(&set.class).ok_or(CaptureError::Incomplete)?,
                namespace: set.namespace.clone(),
                name: set.name.clone(),
            })
        })
        .collect::<Result<_, CaptureError>>()?;
    let retained = sealed
        .scope()
        .retained
        .difference(vanishable)
        .cloned()
        .collect();
    let dropped = sealed
        .identifications()
        .iter()
        .map(|lookup| {
            Ok(DroppedSignature {
                spelled: lookup.signature.clone(),
                path: lookup.search_path.clone(),
                kind: match lookup.kind.as_str() {
                    "f" => "f",
                    "p" => "p",
                    _ => return Err(CaptureError::Incomplete),
                },
            })
        })
        .collect::<Result<_, CaptureError>>()?;
    Ok((
        CaptureScope {
            retained,
            candidates,
        },
        dropped,
    ))
}

fn sealed_ownership(sealed: &InputManifest) -> BTreeMap<ObjectIdentity, ObjectOwnership> {
    sealed
        .prerequisites()
        .iter()
        .map(|p| (p.object.clone(), p.ownership.clone()))
        .collect()
}

#[cfg(all(test, unix))]
mod tests;
