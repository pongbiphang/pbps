//! Probes the permission family of changes before apply (DEC-2790.4).

// The moved arms use the parent preflight's private helpers, which stay there.
use super::*;

pub(super) fn preflight(permission: &GrantChange) -> Result<Vec<Probe>, DialectError> {
    match permission {
        // A permission change fails on nothing in the data; the engine refuses
        // an impossible grant inside the transaction.
        GrantChange::PublicExecution { .. } => Ok(Vec::new()),
        // Except for the one thing a permission change can fail on that is
        // not the permission: the securable. `validate` accepts a schema
        // target it cannot see inside — an external schema has no declared
        // objects — and this tool never creates a schema, so a grant on one
        // the database does not have is a statement the engine refuses.
        // Under `apply --staged` every change before it has committed by
        // then (DECISIONS 134). An *object* target needs no probe: it is
        // declared, so it exists or this plan creates it.
        GrantChange::Grant { role, target, .. } | GrantChange::Revoke { role, target, .. } => {
            match target {
                pbps_model::GrantTarget::Schema(schema) => Ok(vec![schema_probe(role, schema)]),
                // Both are declared objects, so each exists or this plan
                // creates it. A routine target does not reach an apply on this
                // engine at all — `validate::role` refuses it — and probing
                // for it would be a probe for something no plan can hold.
                pbps_model::GrantTarget::Object(_)
                | pbps_model::GrantTarget::Routine(_)
                | pbps_model::GrantTarget::Sequence(_) => Ok(Vec::new()),
            }
        }
    }
}
