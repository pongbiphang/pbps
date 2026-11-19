//! The index family (#2950).

use pbps_dialect::Statement;
use pbps_model::{Index, IndexChange, IndexKey, ObjectLocalName, TableName};

use super::{Sql, closed, local, qualified, statement};
use crate::validate::{self, unsupported};

pub(super) fn emit(change: &IndexChange) -> Sql {
    match change {
        IndexChange::AddIndex {
            table,
            name,
            index,
            clustered,
        } => {
            if *clustered {
                return Err(unsupported("a clustered index"));
            }
            Ok(vec![create(table, name, index)?])
        }
        IndexChange::DropIndex { table, name } => {
            qualified(table)?;
            let index = qualified(&TableName::new(table.schema.clone(), name.raw()))?;
            Ok(vec![statement(["DROP INDEX ", &index].concat())])
        }
        IndexChange::BuildPartitionedIndex(_) => Err(unsupported("partitions")),
        IndexChange::SetIndexStorageParameters { .. } => {
            Err(unsupported("index storage parameters"))
        }
    }
}

/// `CREATE [UNIQUE] INDEX main."i" ON "t" ("a" DESC, ..) [WHERE ..]`. The index
/// is in `main` and so is the table; the table is written bare, because `ON
/// main.t` is a syntax error.
pub(super) fn create(
    table: &TableName,
    name: &ObjectLocalName,
    index: &Index,
) -> Result<Statement, pbps_dialect::DialectError> {
    let mut found = Vec::new();
    validate::index_errors(&table.shown().to_string(), name, index, None, &mut found);
    if let Some(error) = found.into_iter().next() {
        return Err(error);
    }
    let at = qualified(&TableName::new(table.schema.clone(), name.raw()))?;
    qualified(table)?;
    let mut keys = Vec::new();
    for key in &index.columns {
        let IndexKey::Column(column) = &key.key else {
            return Err(validate::not_built("expression indexes"));
        };
        let mut text = local(column.raw())?;
        if key.descending {
            text.push_str(" DESC");
        }
        keys.push(text);
    }
    let mut sql = [
        "CREATE ",
        if index.unique { "UNIQUE " } else { "" },
        "INDEX ",
        &at,
        " ON ",
        &local(&table.name)?,
        " (",
        &keys.join(", "),
        ")",
    ]
    .concat();
    if let Some(filter) = &index.filter {
        sql.push_str(" WHERE ");
        sql.push_str(&closed(filter));
    }
    Ok(statement(sql))
}
