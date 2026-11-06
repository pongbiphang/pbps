//! Apply's captures of a sealed manifest (#2144, #616): the manifest's own
//! scope, read again inside the apply's transaction and sealed under the
//! plan's key, for the caller to compare with the seal.
//!
//! Nothing here decides whether the target still matches; the comparison is
//! the model's, over two manifests sealed by the same rule under the same
//! key. Nothing here holds the catalog still either (DEC-619.1): each
//! component is a plain read, and a commit that reaches a component read
//! later differs there. A read waits for another session's uncommitted DDL
//! in the schema, as the apply's own statements do; no lock timeout is set
//! (DEC-2144.1).

use super::{
    CandidateClass, CandidateSet, CaptureError, CaptureScope, ObjectIdentity, RecordedOwnership,
    capture_declared, ownership,
};
use pbps_model::dependency::Dependencies;
use pbps_model::resolver::{InputManifest, ObjectOwnership};
use std::collections::{BTreeMap, BTreeSet};

/// The opening recheck, before apply's first statement: `sealed`'s scope as
/// the target holds it now, under the declarations the plan carries.
///
/// Each record keeps the ownership the seal proved for it. Ownership follows
/// from the recorded declarations and the record's own catalog facts: apply
/// has already refused a ledger other than the plan's baseline, and a record
/// whose facts moved differs in its properties, so a fresh classification
/// could only repeat the sealed one or refuse what the properties refuse
/// anyway.
pub async fn recheck_opening(
    connection: &mut impl pbps_db::transport::QueryConnection,
    sealed: &InputManifest,
    declarations: &Dependencies,
    key: &pbps_db::fingerprint::EnvironmentFingerprintKey,
) -> Result<InputManifest, CaptureError> {
    let scope = request(sealed, &BTreeSet::new())?;
    let captured = capture_declared(connection, &scope, declarations, key).await?;
    captured
        .require_runtime_candidates()
        .map_err(CaptureError::Coverage)?;
    captured
        .seal(key, &sealed_ownership(sealed))
        .map_err(|_| CaptureError::Incomplete)
}

/// The closing capture, after apply's last statement: `sealed`'s scope as
/// the plan left the target, and the records it holds that the seal does
/// not, each with the ownership `recorded`, the plan's declarations, proves
/// for it (DEC-614.2).
///
/// `vanishable` are the sealed records the plan installs whose names the
/// read must not insist on: left out of the retained roots, they are read
/// where they are found and not refused where they are not (#616). A name
/// the engine chose afresh needs no such care on SQL Server (measured on
/// #2144: an unnamed key, default or check compares equal on scratch and on
/// the target), so the set is usually empty here.
pub async fn capture_closing(
    connection: &mut impl pbps_db::transport::QueryConnection,
    sealed: &InputManifest,
    vanishable: &BTreeSet<ObjectIdentity>,
    declarations: &Dependencies,
    key: &pbps_db::fingerprint::EnvironmentFingerprintKey,
    recorded: &RecordedOwnership<'_>,
) -> Result<InputManifest, CaptureError> {
    let mut scope = request(sealed, vanishable)?;
    // The plan's closing seal retains the columns of the objects it creates:
    // they are read with their object, which is retained beside them. A
    // column whose object is not retained stays, and refuses as any identity
    // the capture does not look up (the opening seal retains none).
    scope.retained.retain(|id| {
        !(id.class == super::class::COLUMN
            && id.signature.len() == 1
            && sealed.scope().retained.contains(&id.signature[0]))
    });
    let captured = capture_declared(connection, &scope, declarations, key).await?;
    captured
        .require_runtime_candidates()
        .map_err(CaptureError::Coverage)?;
    let mut ownership = ownership::classify(&captured, recorded).map_err(CaptureError::Coverage)?;
    // A record the seal holds is compared under the seal's ownership; the
    // fresh classification speaks only for the records it does not.
    ownership.extend(sealed_ownership(sealed));
    captured
        .seal(key, &ownership)
        .map_err(|_| CaptureError::Incomplete)
}

/// The deployer's authorization as the target answers it for the questions
/// the plan's seal asked (#2144): the externals are derived from what the
/// sealed manifests proved managed, by the derivation planning used, and the
/// inventory over them is built by the one function planning used. The
/// result is what a sealed `before` (read now) or `after` (read after the
/// plan's last statement) digest is of.
///
/// `read` is the bracketed scope read of the target's deployer; `sealed`
/// are the plan's opening and closing manifests, whose managed objects
/// together are the managed set planning read from the declarations.
pub async fn answered_authorization(
    connection: &mut impl pbps_db::transport::QueryConnection,
    read: &crate::resolver::authorization::AuthorizationContext,
    planned: &[pbps_model::RoleName],
    sealed: [&InputManifest; 2],
    declarations: &Dependencies,
    key: &pbps_db::fingerprint::EnvironmentFingerprintKey,
) -> Result<crate::resolver::authorization::AuthorizationContext, pbps_db::DbError> {
    use crate::resolver::authorization::{InventoryError, answered_context, staged_inventory};
    let refused = |what: &str, error: &dyn std::fmt::Display| {
        pbps_db::DbError::Refused(format!("{what}: {error}"))
    };
    let managed: BTreeSet<ObjectIdentity> = sealed
        .iter()
        .flat_map(|manifest| manifest.prerequisites())
        .filter(|record| matches!(record.ownership, ObjectOwnership::Surface(_)))
        .map(|record| record.object.clone())
        .collect();
    let scope = request(sealed[0], &BTreeSet::new())
        .map_err(|error| refused("the sealed scope", &error))?;
    let captured = capture_declared(connection, &scope, declarations, key)
        .await
        .map_err(|error| refused("the externals the plan was sealed over", &error))?;
    let external = super::ExternalInputs::of(captured, managed);
    let expected = staged_inventory(connection, read, planned, &external, declarations, key)
        .await
        .map_err(|error| match error {
            InventoryError::Read(error) => error,
            InventoryError::Capture(error) => refused("the staged inventory", &error),
            InventoryError::Owners(why) => pbps_db::DbError::Refused(why),
        })?;
    answered_context(connection, &expected).await
}

/// What `sealed` asked its read for, less `vanishable` among its roots. A
/// candidate catalog or a signature lookup this build does not read refuses:
/// it would be read as nothing.
fn request(
    sealed: &InputManifest,
    vanishable: &BTreeSet<ObjectIdentity>,
) -> Result<CaptureScope, CaptureError> {
    // SQL Server holds no DROP-signature lookups; a seal that carries one
    // was not made by this capture.
    if !sealed.identifications().is_empty() {
        return Err(CaptureError::Incomplete);
    }
    let candidates = sealed
        .scope()
        .candidates
        .iter()
        .map(|set| {
            Ok(CandidateSet {
                class: CandidateClass::of_catalog(&set.class).ok_or(CaptureError::Incomplete)?,
                schema: set.namespace.clone().ok_or(CaptureError::Incomplete)?,
                name: set.name.clone().ok_or(CaptureError::Incomplete)?,
            })
        })
        .collect::<Result<_, CaptureError>>()?;
    let retained = sealed
        .scope()
        .retained
        .difference(vanishable)
        .cloned()
        .collect();
    Ok(CaptureScope {
        retained,
        candidates,
    })
}

fn sealed_ownership(sealed: &InputManifest) -> BTreeMap<ObjectIdentity, ObjectOwnership> {
    sealed
        .prerequisites()
        .iter()
        .map(|p| (p.object.clone(), p.ownership.clone()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::CandidateClass;

    #[test]
    fn a_candidate_class_round_trips_through_its_catalog_and_an_unknown_one_is_none() {
        for class in [
            CandidateClass::Object,
            CandidateClass::SystemProcedure,
            CandidateClass::Type,
        ] {
            assert_eq!(CandidateClass::of_catalog(class.catalog()), Some(class));
        }
        // A catalog this build does not read, a near miss and an empty name
        // are no class: each would be read as a candidate that never arrives.
        for unknown in ["pg_class", "sys.tables", "SYS.OBJECTS", "sys.objects ", ""] {
            assert_eq!(CandidateClass::of_catalog(unknown), None, "{unknown:?}");
        }
    }
}
