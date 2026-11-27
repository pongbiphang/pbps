//! A recorded attach rehearsal held against the database (#1671, DEC-1671.2):
//! the two relations' indexes read from the catalog, and compared with the
//! plan's record before the first statement and again before the `ATTACH`.
//!
//! The engine takes, for each index of the parent's in the order they were
//! made, the first of the table's in that order which it holds equal and which
//! nothing has taken. So the order is part of what is compared.

use crate::{catalog, emit};
use pbps_db::{Conn, DbError};
use pbps_model::adoption::{HeldIndex, Relation, RelationIndex, Relations, difference, standing};
use pbps_model::{Change, ChangeSet, PartitionChange, TableName};

/// One row for each relation of `wanted` that is a table, with its indexes in
/// the order of their object numbers, which is the order the engine looks
/// through them in. Spelled by the session's path, so it is only ever run under
/// the catalog's canonical one.
pub(crate) fn query<'a>(wanted: impl IntoIterator<Item = &'a TableName>) -> String {
    let named = wanted
        .into_iter()
        .map(|table| {
            let (schema, name) = (emit::literal(&table.schema), emit::literal(&table.name));
            format!("({schema}, {name})")
        })
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "SELECT n.nspname::pg_catalog.text AS schema_name,
                c.relname::pg_catalog.text AS table_name,
                COALESCE((SELECT pg_catalog.json_agg(pg_catalog.json_build_object(
                                     'name', ic.relname,
                                     'index', pg_catalog.pg_get_indexdef(i.indexrelid),
                                     'constraint', pg_catalog.pg_get_constraintdef(k.oid),
                                     'valid', i.indisvalid)
                                 ORDER BY i.indexrelid)
                            FROM pg_catalog.pg_index i
                            JOIN pg_catalog.pg_class ic ON ic.oid = i.indexrelid
                            LEFT JOIN pg_catalog.pg_constraint k
                              ON k.conindid = i.indexrelid AND k.conrelid = i.indrelid
                             AND k.contype IN ('p', 'u', 'x')
                           WHERE i.indrelid = c.oid), '[]'::pg_catalog.json) AS indexes
           FROM pg_catalog.pg_class c
           JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
          WHERE c.relkind IN ('r', 'p')
            AND (n.nspname::pg_catalog.text, c.relname::pg_catalog.text) IN (VALUES {named})"
    )
}

#[derive(serde::Deserialize)]
struct Row {
    schema_name: String,
    table_name: String,
    indexes: Vec<Index>,
}

#[derive(serde::Deserialize)]
struct Index {
    name: String,
    index: String,
    constraint: Option<String>,
    valid: bool,
}

/// The rows of [`query`]. A wanted relation with no row is not in the catalog;
/// a row that cannot be read is an error, never that.
pub(crate) fn decode<'a>(
    wanted: impl IntoIterator<Item = &'a TableName>,
    rows: &[serde_json::Value],
) -> Result<Relations, DbError> {
    let mut found: Relations = wanted.into_iter().map(|t| (t.clone(), None)).collect();
    for row in rows {
        let row: Row = serde_json::from_value(row.clone())
            .map_err(|e| DbError::BadRow(format!("invalid attach relation: {e}")))?;
        let slot = found
            .get_mut(&TableName::new(&row.schema_name, &row.table_name))
            .filter(|slot| slot.is_none())
            .ok_or_else(|| DbError::BadRow("an attach relation was not asked for".into()))?;
        *slot = Some(Relation {
            indexes: row
                .indexes
                .into_iter()
                .map(|i| HeldIndex {
                    index: RelationIndex {
                        name: i.name.into(),
                        index: i.index.into(),
                        constraint: i.constraint.map(Into::into),
                    },
                    valid: i.valid,
                })
                .collect(),
        });
    }
    Ok(found)
}

/// The relations as they stand, spelled as a record spells them: under the
/// canonical path, inside a savepoint of the caller's transaction, which leaves
/// the session's settings as they were.
async fn read(conn: &mut Conn, wanted: &[&TableName]) -> Result<Relations, DbError> {
    let path = catalog::canonical_path_of(conn)?;
    let sql = format!(
        "SELECT COALESCE(pg_catalog.jsonb_agg(pg_catalog.to_jsonb(r)), '[]'::pg_catalog.jsonb)\
         ::pg_catalog.text AS payload FROM ({}) r",
        query(wanted.iter().copied())
    );
    conn.execute("SAVEPOINT pbps_adoption").await?;
    let rows = async {
        conn.query(&path).await?;
        conn.query(&sql).await
    }
    .await;
    conn.execute("ROLLBACK TO SAVEPOINT pbps_adoption; RELEASE SAVEPOINT pbps_adoption")
        .await?;
    let rows = rows?;
    let payload = rows
        .first()
        .and_then(|row| row.try_get::<&str>("payload").ok().flatten())
        .ok_or_else(|| DbError::BadRow("the attach relations could not be read".into()))?;
    let payload: Vec<serde_json::Value> = serde_json::from_str(payload)
        .map_err(|e| DbError::BadRow(format!("invalid attach relations: {e}")))?;
    decode(wanted.iter().copied(), &payload)
}

/// Compares each relation with what the record says it holds.
async fn compare(
    conn: &mut Conn,
    expected: [(&TableName, Vec<RelationIndex>); 2],
) -> Result<(), DbError> {
    let found = read(conn, &[expected[0].0, expected[1].0]).await?;
    for (name, indexes) in &expected {
        let how = match found.get(*name).and_then(Option::as_ref) {
            Some(relation) => difference(indexes, &relation.indexes),
            None => Some("it is not there".to_owned()),
        };
        if let Some(how) = how {
            return Err(DbError::Refused(format!(
                "`{name}` is not as the attach's rehearsal read it: {how}. Which of the table's \
                 indexes the attach takes was decided on that reading, so it is not run. Restore \
                 the indexes, or plan again with `pbps plan --db`.",
                name = name.shown()
            )));
        }
    }
    Ok(())
}

/// Before any statement of an apply: every relation of every recorded
/// rehearsal, against the whole record.
pub async fn check(conn: &mut Conn, changes: &ChangeSet) -> Result<(), DbError> {
    for planned in &changes.changes {
        if let Change::Partition(PartitionChange::AttachPartition {
            table,
            parent,
            rehearsal: Some(rehearsal),
            ..
        }) = &planned.change
        {
            compare(
                conn,
                [
                    (parent, rehearsal.parent.clone()),
                    (table, rehearsal.table.clone()),
                ],
            )
            .await?;
        }
    }
    Ok(())
}

/// Just before one attach, in its transaction and under the locks it takes, the
/// parent's first as the statement takes them: the record less what the plan has
/// dropped by now.
pub async fn hold(conn: &mut Conn, changes: &ChangeSet, attach: &Change) -> Result<(), DbError> {
    let at = changes.changes.iter().position(|p| p.change == *attach);
    let (
        Some(at),
        Change::Partition(PartitionChange::AttachPartition {
            table,
            parent,
            rehearsal: Some(rehearsal),
            ..
        }),
    ) = (at, attach)
    else {
        return Err(DbError::Refused(
            "the statement's attach is not one of the plan's recorded rehearsals".into(),
        ));
    };
    let refused = |e: pbps_dialect::DialectError| DbError::Refused(e.to_string());
    conn.execute(&format!(
        "LOCK TABLE ONLY {} IN SHARE UPDATE EXCLUSIVE MODE; \
         LOCK TABLE ONLY {} IN ACCESS EXCLUSIVE MODE;",
        emit::qualified(parent).map_err(refused)?,
        emit::qualified(table).map_err(refused)?
    ))
    .await?;
    compare(
        conn,
        [
            (parent, standing(changes, at, parent, &rehearsal.parent)),
            (table, standing(changes, at, table, &rehearsal.table)),
        ],
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(name: &str) -> serde_json::Value {
        serde_json::json!({
            "schema_name": "app", "table_name": name,
            "indexes": [{"name": "i", "index": "CREATE INDEX i", "constraint": null, "valid": false}],
        })
    }

    #[test]
    fn a_relation_the_catalog_lacks_an_empty_one_and_an_unreadable_one_differ() {
        let wanted = [TableName::new("app", "c"), TableName::new("app", "p")];
        let mut empty = row("c");
        empty["indexes"] = serde_json::json!([]);
        let found = decode(&wanted, &[empty]).unwrap();
        assert!(found[&wanted[0]].as_ref().unwrap().indexes.is_empty());
        assert!(found[&wanted[1]].is_none());
        let found = decode(&wanted, &[row("c")]).unwrap();
        assert!(!found[&wanted[0]].as_ref().unwrap().indexes[0].valid);
        for column in ["indexes", "table_name"] {
            let mut broken = row("c");
            broken.as_object_mut().unwrap().remove(column);
            assert!(decode(&wanted, &[broken]).is_err(), "{column}");
        }
        assert!(decode(&wanted, &[row("other")]).is_err());
        assert!(decode(&wanted, &[row("c"), row("c")]).is_err());
    }
}
