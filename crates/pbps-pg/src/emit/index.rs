//! Emits the index family of changes (DEC-2790.4).

// The moved arms use the parent emitter's private helpers, which stay there.
use super::*;

pub(super) fn emit(pg: &Postgres, change: &Change, index: &IndexChange, strategy: Strategy) -> Sql {
    match index {
        IndexChange::BuildPartitionedIndex(recipe) => rebuild::build_partitioned_index(pg, recipe),
        IndexChange::SetIndexStorageParameters {
            table,
            target,
            method,
            set,
            reset,
        } => {
            let actions = storage_actions(set, reset, &|name| {
                method.policy().parameter(name).is_some()
            })?;
            let alter = |index: &str| -> Result<String, DialectError> {
                Ok(format!(
                    "ALTER INDEX {}.{} {actions};",
                    quote(&table.schema)?,
                    quote(index)?
                ))
            };
            match target {
                // A unique or exclusion constraint's index carries the
                // constraint's name (measured on 16.15 and 18.6).
                pbps_model::IndexPart::Unique(n)
                | pbps_model::IndexPart::Index(n)
                | pbps_model::IndexPart::Exclusion(n) => one(pg, table, alter(n.raw())?),
                // The key's index is found when the statement runs, as an
                // unnamed key's drop is: a pulled key carries its name, but a
                // declared one need not (#1442).
                pbps_model::IndexPart::PrimaryKey => {
                    let q = qualified(table)?;
                    let body = format!(
                        "DECLARE ix name := (SELECT i.relname FROM pg_catalog.pg_constraint k\n\
                         \x20                    JOIN pg_catalog.pg_class i ON i.oid = k.conindid\n\
                         \x20                    WHERE k.conrelid = {}::pg_catalog.regclass AND k.contype = 'p');\n\
                         BEGIN\n\
                         \x20   EXECUTE pg_catalog.format({}, ix);\n\
                         END",
                        literal(&q),
                        literal(&format!(
                            "ALTER INDEX {}.%I {}",
                            quote(&table.schema)?.replace('%', "%%"),
                            actions.replace('%', "%%")
                        ))
                    );
                    let tag = dollar_tag(&body);
                    unscoped(format!("DO {tag}\n{body}\n{tag};"))
                }
            }
        }
        IndexChange::AddIndex { table, .. } if change_is_clustered(change) => {
            Err(no_clustered_layout(table))
        }
        IndexChange::AddIndex {
            table, name, index, ..
        } => {
            let sql = create_index(table, name.raw(), index, strategy)?;
            if built_concurrently(index, strategy) {
                // Alone in its batch and outside the transaction, because the
                // engine says so in as many words: `CREATE INDEX CONCURRENTLY
                // cannot run inside a transaction block`. Saying it here rather
                // than in the runner is what lets a plan carrying one be
                // refused at plan time, with the whole plan intact, instead of
                // halfway through an apply.
                Ok(vec![
                    Statement::new(sql)
                        .own_batch()
                        .non_transactional()
                        .building_index(table.clone(), name),
                ])
            } else {
                Ok(vec![on(pg, table, &sql)?])
            }
        }
        // No `CONCURRENTLY` on the drop, deliberately. It would make the
        // statement non-transactional — and so the whole plan — to spare an
        // `ACCESS EXCLUSIVE` lock held for a catalog update, which is the one
        // part of an index's life that is not proportional to the table.
        IndexChange::DropIndex { table, name } => one(
            pg,
            table,
            format!(
                "DROP INDEX {}.{};",
                quote(&table.schema)?,
                quote(name.raw())?
            ),
        ),
    }
}
