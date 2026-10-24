//! Object-visible reconstruction inputs. Target access is catalog queries only.

use super::{Limitation, ObjectName};
use crate::resolver::scratch::{ident, literal, one, text};
use pbps_db::{DbError, transport::QueryConnection};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub(super) struct Column {
    pub name: String,
    pub column_id: i32,
    pub type_name: String,
    pub type_schema: String,
    pub user_defined: bool,
    pub max_length: i16,
    pub precision: u8,
    pub scale: u8,
    pub collation: Option<String>,
    pub nullable: bool,
    pub identity: bool,
    pub computed: bool,
    pub computed_definition: Option<String>,
    pub persisted: Option<bool>,
    pub seed: Option<String>,
    pub increment: Option<String>,
    pub not_for_replication: Option<bool>,
    pub generated_always: u8,
    pub hidden: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub(super) struct Relation {
    pub kind: String,
    pub readable: bool,
    pub definition: Option<String>,
    pub ansi_nulls: Option<bool>,
    pub quoted_identifier: Option<bool>,
    pub schema_bound: Option<bool>,
    pub columns: Vec<Column>,
    pub triggers: Vec<Trigger>,
    pub indexes: Vec<super::indexes::Index>,
    #[serde(skip)]
    pub references: BTreeSet<ObjectName>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub(super) struct Trigger {
    pub name: String,
    pub verb: String,
    pub disabled: bool,
}

#[derive(Deserialize)]
struct Reference {
    server: Option<String>,
    database: Option<String>,
    schema: Option<String>,
    name: String,
    class: u8,
    caller_dependent: bool,
}

pub(super) struct Closure {
    pub root: Relation,
    pub relations: BTreeMap<ObjectName, Relation>,
    pub limitation: Option<Limitation>,
}

fn query_in(database: &str, select: &str) -> String {
    // A fixed catalog SELECT is the only executable input to this wrapper.
    // The view's returned definition is never sent back through the target.
    format!(
        "EXEC ({});",
        literal(&format!("USE {}; {select}", ident(database)))
    )
}

fn relation_query(name: &ObjectName) -> String {
    let local = literal(&format!("{}.{}", ident(&name.schema), ident(&name.name)));
    format!(
        r#"SELECT CONVERT(int,COALESCE(HAS_PERMS_BY_NAME({local},N'OBJECT',N'VIEW DEFINITION'),0)) AS readable,
(SELECT RTRIM(o.type) AS kind,
CONVERT(bit,COALESCE(HAS_PERMS_BY_NAME({local},N'OBJECT',N'VIEW DEFINITION'),0)) AS readable,
m.definition,m.uses_ansi_nulls AS ansi_nulls,m.uses_quoted_identifier AS quoted_identifier,m.is_schema_bound AS schema_bound,
JSON_QUERY(COALESCE((SELECT c.name,c.column_id,t.name AS type_name,s.name AS type_schema,
t.is_user_defined AS user_defined,c.max_length,c.precision,c.scale,c.collation_name AS collation,
c.is_nullable AS nullable,c.is_identity AS [identity],c.is_computed AS computed,
cc.definition AS computed_definition,cc.is_persisted AS persisted,
CONVERT(nvarchar(100),ic.seed_value) AS seed,CONVERT(nvarchar(100),ic.increment_value) AS increment,
ic.is_not_for_replication AS not_for_replication,c.generated_always_type AS generated_always,c.is_hidden AS hidden
FROM sys.columns c JOIN sys.types t ON t.user_type_id=c.user_type_id
JOIN sys.schemas s ON s.schema_id=t.schema_id
LEFT JOIN sys.computed_columns cc ON cc.object_id=c.object_id AND cc.column_id=c.column_id
LEFT JOIN sys.identity_columns ic ON ic.object_id=c.object_id AND ic.column_id=c.column_id
WHERE c.object_id=o.object_id ORDER BY c.column_id FOR JSON PATH,INCLUDE_NULL_VALUES),N'[]')) AS columns,
JSON_QUERY(COALESCE((SELECT tr.name,e.type_desc AS verb,tr.is_disabled AS disabled
FROM sys.triggers tr JOIN sys.trigger_events e ON e.object_id=tr.object_id
WHERE tr.parent_id=o.object_id AND tr.is_instead_of_trigger=1
ORDER BY tr.name,e.type_desc FOR JSON PATH,INCLUDE_NULL_VALUES),N'[]')) AS triggers,
JSON_QUERY(COALESCE((SELECT i.index_id AS id,i.name,i.type AS kind,i.is_unique AS [unique],
i.is_disabled AS disabled,i.has_filter,
JSON_QUERY(COALESCE((SELECT c.name,ic.key_ordinal AS ordinal,
ic.is_descending_key AS descending,ic.is_included_column AS included
FROM sys.index_columns ic LEFT JOIN sys.columns c ON c.object_id=ic.object_id AND c.column_id=ic.column_id
WHERE ic.object_id=i.object_id AND ic.index_id=i.index_id AND (ic.key_ordinal>0 OR ic.is_included_column=1)
ORDER BY ic.is_included_column,ic.key_ordinal,ic.index_column_id
FOR JSON PATH,INCLUDE_NULL_VALUES),N'[]')) AS columns
FROM sys.indexes i WHERE i.object_id=o.object_id AND o.type=N'V' AND i.index_id>0
ORDER BY i.name FOR JSON PATH,INCLUDE_NULL_VALUES),N'[]')) AS indexes
FROM sys.objects o LEFT JOIN sys.sql_modules m ON m.object_id=o.object_id
WHERE o.object_id=OBJECT_ID({local})
FOR JSON PATH,INCLUDE_NULL_VALUES) AS payload;"#
    )
}

fn references_query(name: &ObjectName) -> String {
    let local = literal(&format!("{}.{}", ident(&name.schema), ident(&name.name)));
    format!(
        "SELECT COALESCE((SELECT referenced_server_name AS server,referenced_database_name AS [database],\
         referenced_schema_name AS [schema],referenced_entity_name AS name,referenced_class AS class,\
         is_caller_dependent AS caller_dependent FROM sys.dm_sql_referenced_entities({local},N'OBJECT') \
         FOR JSON PATH,INCLUDE_NULL_VALUES),N'[]') AS payload;"
    )
}

async fn json<T: serde::de::DeserializeOwned>(
    target: &mut impl QueryConnection,
    name: &ObjectName,
    query: &str,
) -> Result<T, DbError> {
    let row = one(
        target.query(&query_in(&name.database, query)).await?,
        "dependency metadata",
    )?;
    serde_json::from_str(&text(&row, "payload")?)
        .map_err(|e| DbError::BadRow(format!("dependency metadata: {e}")))
}

pub(super) async fn relation(
    target: &mut impl QueryConnection,
    name: &ObjectName,
) -> Result<Option<Relation>, DbError> {
    let row = one(
        target
            .query(&query_in(&name.database, &relation_query(name)))
            .await?,
        "dependency visibility",
    )?;
    if row.try_get::<i32>("readable")? != Some(1) {
        return Ok(None);
    }
    let mut rows: Vec<Relation> = serde_json::from_str(&text(&row, "payload")?)
        .map_err(|e| DbError::BadRow(format!("dependency metadata: {e}")))?;
    if rows.len() > 1 {
        return Err(DbError::BadRow(
            "dependency object lookup was ambiguous".into(),
        ));
    }
    Ok(rows.pop().filter(|r| r.readable))
}

pub(super) async fn closure(
    target: &mut impl QueryConnection,
    name: &ObjectName,
    root: Relation,
) -> Result<Closure, DbError> {
    let mut pending = vec![(name.clone(), root.clone())];
    let mut relations = BTreeMap::new();
    let mut limitation = None;
    while let Some((name, mut value)) = pending.pop() {
        if relations.contains_key(&name) {
            continue;
        }
        if !matches!(value.kind.as_str(), "U" | "V") {
            limitation = Some(Limitation::UnsupportedObject {
                object: name,
                kind: value.kind,
            });
            break;
        }
        let references: Vec<Reference> = match json(target, &name, &references_query(&name)).await {
            Ok(rows) => rows,
            Err(error)
                if error.server_error_code().is_some() || matches!(error, DbError::BadRow(_)) =>
            {
                limitation = Some(Limitation::CatalogRead {
                    object: name,
                    code: error.server_error_code(),
                });
                break;
            }
            Err(error) => return Err(error),
        };
        for reference in references {
            let address = ObjectName {
                database: reference.database.unwrap_or_else(|| name.database.clone()),
                schema: reference.schema.unwrap_or_default(),
                name: reference.name,
            };
            if reference.server.is_some()
                // An INDEX hint's class-7 reference names its owning
                // relation; view index facets are read with that object.
                || !matches!(reference.class, 1 | 7)
                || reference.caller_dependent
                || address.schema.is_empty()
            {
                limitation = Some(Limitation::UnresolvedReference { object: address });
                break;
            }
            // Computed columns can report other columns of their own table.
            if address == name && value.kind == "U" {
                continue;
            }
            value.references.insert(address.clone());
            if relations.contains_key(&address) || pending.iter().any(|(n, _)| n == &address) {
                continue;
            }
            match relation(target, &address).await {
                Ok(Some(relation)) => pending.push((address, relation)),
                Ok(None) => {
                    limitation = Some(Limitation::HiddenObject { object: address });
                    break;
                }
                Err(error)
                    if matches!(error.server_error_code().as_deref(), Some("229" | "916")) =>
                {
                    limitation = Some(Limitation::HiddenObject { object: address });
                    break;
                }
                Err(error) => return Err(error),
            }
        }
        if limitation.is_some() {
            break;
        }
        relations.insert(name, value);
    }
    Ok(Closure {
        root,
        relations,
        limitation,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn target_catalog_queries_never_compile_a_write_or_an_execution_mode() {
        let object = ObjectName {
            database: "db']".into(),
            schema: "dbo".into(),
            name: "v']".into(),
        };
        for query in [relation_query(&object), references_query(&object)] {
            let upper = query.to_ascii_uppercase();
            for forbidden in [
                "INSERT",
                "UPDATE",
                "DELETE",
                "MERGE",
                "NOEXEC",
                "PARSEONLY",
                "FMTONLY",
                "DM_EXEC_DESCRIBE_FIRST_RESULT_SET",
            ] {
                assert!(!upper.contains(forbidden), "{query}");
            }
            assert!(query.starts_with("SELECT "));
            assert!(
                query_in(&object.database, &query).starts_with("EXEC (N'USE [db'']]]; SELECT ")
            );
        }
    }
}
