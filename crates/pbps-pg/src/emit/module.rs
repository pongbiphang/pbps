//! Emits the module family of changes (DEC-2790.4).

// The moved arms use the parent emitter's private helpers, which stay there.
use super::*;

pub(super) fn emit(pg: &Postgres, module: &ModuleChange) -> Sql {
    match module {
        // A module is created, never replaced. `CREATE OR REPLACE` exists on
        // this engine and buys nothing: measured, it refuses a changed return
        // type and a reordered view column, and it drops `reloptions` just as a
        // rebuild does — so the cheap path is neither cheap nor complete, and
        // *which* edits it can express is decided by text §8.2 forbids parsing.
        // One shape, always (ADR-0009 §3).
        ModuleChange::CreateModule { id, module } => Ok(vec![create_module(pg, id, module)?]),
        // Two statements, not one, so that the plan a human approves says
        // `DROP` where a `DROP` will run. Everything the catalog attached to
        // the old object goes with it, and carrying that across is the
        // connected plan's obligation, not the emitter's: only a connection can
        // see an ACL, an owner or a `reloptions` (ADR-0009 §3).
        ModuleChange::AlterModule { id, module } => Ok(vec![
            drop_module(pg, id, module.kind)?,
            create_module(pg, id, module)?,
        ]),
        ModuleChange::DropModule { id, kind } => Ok(vec![drop_module(pg, id, *kind)?]),
    }
}
