//! Probes the permission family of changes before apply (DEC-2790.4).

// The moved arms use the parent preflight's private helpers, which stay there.
use super::*;

pub(super) fn preflight(permission: &GrantChange) -> Result<Vec<Probe>, DialectError> {
    match permission {
        GrantChange::Grant { .. }
        | GrantChange::Revoke { .. }
        | GrantChange::PublicExecution { .. } => Ok(Vec::new()),
    }
}
