//! Creation-order hints, never catalog binding evidence (DEC-2541.1).
use super::*;
use crate::resolver::capture::ExternalInputs;

#[allow(clippy::wildcard_enum_match_arm)]
fn structural(change: &Change, external: &ObjectIdentity) -> bool {
    let referenced = |name: &pbps_model::TableName| object(&name.schema, &name.name) == *external;
    match change {
        Change::CreateModule { id, .. } => id.attached_to().is_some_and(referenced),
        Change::AddForeignKey { constraint, .. } => referenced(&constraint.references_table),
        Change::CreateTable { table, .. } => table
            .foreign_keys
            .values()
            .any(|fk| referenced(&fk.references_table)),
        _ => false,
    }
}

/// Only finite logical operation IDs leave this function. No SQL or tokens
/// survive in the blueprint, and the final scratch read remains authoritative.
pub(super) fn before(
    steps: &[Draft],
    inputs: &super::super::super::capture::ExternalInputs,
    external: &ObjectIdentity,
) -> Result<Vec<Node>, ReconstructError> {
    let candidates: Vec<_> = inputs
        .roots()
        .iter()
        .chain(steps.iter().flat_map(|step| &step.metadata.created))
        .collect();
    steps
        .iter()
        .filter_map(|step| match needs(step, external, &candidates) {
            Ok(true) => Some(Ok(Node::Operation(step.operation))),
            Ok(false) => None,
            Err(error) => Some(Err(error)),
        })
        .collect()
}

#[allow(clippy::wildcard_enum_match_arm)]
fn needs(
    step: &Draft,
    external: &ObjectIdentity,
    candidates: &[&ObjectIdentity],
) -> Result<bool, ReconstructError> {
    let change = &step.change.change;
    if structural(change, external) {
        return Ok(true);
    }
    let refusal = |reason: &str| {
        ReconstructError::Unsupported(format!(
            "{} relative to external {external:?}: {reason}",
            step.metadata.declaration,
        ))
    };
    let (names, home) = match change {
        Change::CreateModule { id, module } => (
            ExternalInputs::module_names(module).map_err(refusal)?,
            id.schema(),
        ),
        _ => {
            let emitted = crate::emit::emit(change, step.change.strategy).map_err(|error| {
                ReconstructError::Emit {
                    declaration: step.metadata.declaration.clone(),
                    reason: error.to_string(),
                }
            })?;
            let text = emitted
                .iter()
                .map(|statement| statement.sql.as_str())
                .collect::<Vec<_>>()
                .join(";\n");
            (ExternalInputs::statement_names(&text).map_err(refusal)?, "")
        }
    };
    let candidates: Vec<_> = candidates
        .iter()
        .copied()
        .filter(|id| !step.metadata.created.contains(id))
        .collect();
    Ok(names.needs(home, external, &candidates))
}

#[cfg(test)]
mod tests;
