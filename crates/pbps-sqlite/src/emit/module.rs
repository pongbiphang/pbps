//! The module family: views, the only module SQLite has (#2950).

use pbps_dialect::Released;
use pbps_model::{Module, ModuleChange, ModuleId, ModuleKind, TableName};

use super::{Sql, closed, qualified, statement};
use crate::validate::{self, not_built, unsupported};

pub(super) fn emit(change: &ModuleChange) -> Sql {
    match change {
        ModuleChange::CreateModule { id, module } => Ok(vec![create(id, module)?]),
        // Two statements: `CREATE OR REPLACE VIEW` is a syntax error. Between
        // them the name is free, which the first statement says it releases.
        ModuleChange::AlterModule { id, module } => {
            let create = create(id, module)?;
            Ok(vec![drop(id)?, create])
        }
        ModuleChange::DropModule { id, kind } => match kind {
            ModuleKind::View => Ok(vec![drop(id)?]),
            ModuleKind::Procedure => Err(unsupported("procedures")),
            ModuleKind::Function => Err(unsupported(
                "functions; application-defined functions live in the host process",
            )),
            ModuleKind::Trigger => Err(not_built("triggers")),
        },
    }
}

fn name_of(id: &ModuleId) -> TableName {
    let name = id.object_name();
    TableName::new(name.schema, name.name)
}

fn create(
    id: &ModuleId,
    module: &Module,
) -> Result<pbps_dialect::Statement, pbps_dialect::DialectError> {
    // The declaration's own refusals: a kind SQLite lacks, a second statement,
    // a body that does not start like a query.
    if let Some(error) = validate::module(id, module).into_iter().next() {
        return Err(error);
    }
    let sql = [
        "CREATE VIEW ",
        &qualified(&name_of(id))?,
        " AS ",
        &closed(&module.definition),
    ]
    .concat();
    Ok(statement(sql))
}

fn drop(id: &ModuleId) -> Result<pbps_dialect::Statement, pbps_dialect::DialectError> {
    let sql = ["DROP VIEW ", &qualified(&name_of(id))?].concat();
    Ok(statement(sql).releasing(Released::Module(id.clone())))
}
