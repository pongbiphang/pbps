//! Probes the row family of changes before apply (DEC-2790.4).

// The moved arms use the parent preflight's private helpers, which stay there.
use super::*;

pub(super) fn preflight(row: &RowChange) -> Result<Vec<Probe>, DialectError> {
    match row {
        // The whole content of a declared row is in the plan, and anything the
        // engine refuses about it rolls the plan back.
        RowChange::InsertRow { .. } | RowChange::UpdateRow { .. } | RowChange::SetDataMode { .. } | RowChange::SetEnsureKeys { .. } | // The pre-delete probe of ADR-0004 is built by `probes` itself, beside
// the refusals that go with it.
RowChange::DeleteRow { .. } => Ok(Vec::new()),
    }
}
