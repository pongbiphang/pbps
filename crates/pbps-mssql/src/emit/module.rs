//! Emits the module family of changes (DEC-2790.4).

// The moved arms use the parent emitter's private helpers, which stay there.
use super::*;

pub(super) fn emit(module: &ModuleChange) -> Sql {
    match module {
        // `CREATE OR ALTER` (2016 SP1+) rather than drop + create, and not only
        // because it is idempotent: it **preserves the permissions** granted on
        // the object, which drop + create silently destroys (ADR-0002).
        ModuleChange::CreateModule { id, module } | ModuleChange::AlterModule { id, module } => {
            let mut statements = vec![Statement::new(module_definition(id, module)?).own_batch()];
            statements.extend(view_indexes(id, module)?);
            Ok(statements)
        }
        // The object's own name, without a signature: nothing overloads on
        // this engine (ADR-0009 §1), so `DROP FUNCTION app.f` names exactly
        // one object — and a signature in the statement is a syntax error.
        ModuleChange::DropModule { id, kind } => Ok(vec![
            Statement::new(format!(
                "DROP {} {};",
                keyword(*kind),
                qualified(&id.object_name())?
            ))
            .releasing(pbps_dialect::Released::Module(id.clone())),
        ]),
    }
}
