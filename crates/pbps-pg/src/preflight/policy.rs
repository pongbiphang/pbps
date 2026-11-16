//! Probes the policy family of changes before apply (DEC-2790.4).

// The moved arms use the parent preflight's private helpers, which stay there.
use super::*;

pub(super) fn preflight(policy: &PolicyChange) -> Result<Vec<Probe>, DialectError> {
    match policy {
        PolicyChange::DropPolicy { .. }
        | PolicyChange::CreatePolicy { .. }
        | PolicyChange::SetRowSecurity { .. } => Ok(Vec::new()),
    }
}
