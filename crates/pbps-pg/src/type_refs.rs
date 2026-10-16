//! Prerequisites of columns that reference existing types (DEC-1523.1–3).

use std::collections::BTreeSet;

use pbps_db::{Conn, DbError};
use pbps_model::{Change, ChangeSet, ColumnType, Schema, TypePin, TypeRef};

use crate::pins::Within;
use crate::user_types::{self, TypeRead, UserType};

fn failed(why: impl Into<String>) -> DbError {
    DbError::BadRow(why.into())
}

pub fn references(schemas: &[&Schema], changes: &ChangeSet) -> BTreeSet<TypeRef> {
    let mut types: Vec<&ColumnType> = schemas
        .iter()
        .flat_map(|s| s.tables.values())
        .flat_map(|t| t.columns.values().map(|c| &c.ty))
        .collect();
    for planned in &changes.changes {
        match &planned.change {
            Change::CreateTable { table, .. }
            | Change::AttachPartition { shape: table, .. }
            | Change::DetachPartition { shape: table, .. } => {
                types.extend(table.columns.values().map(|c| &c.ty));
            }
            Change::AddColumn { column, .. } => types.push(&column.ty),
            Change::AlterColumnType { from, to, .. } => types.extend([from, to]),
            Change::AlterColumnNullability { ty, .. } => types.push(ty),
            Change::DropTable { .. }
            | Change::RenameTable { .. }
            | Change::DropColumn { .. }
            | Change::RenameColumn { .. }
            | Change::AlterColumnDefault { .. }
            | Change::AlterColumnExpression { .. }
            | Change::AlterColumnNumbering { .. }
            | Change::AddComputedColumn { .. }
            | Change::DropComputedColumn { .. }
            | Change::SetColumnDeprecated { .. }
            | Change::SetReplicaIdentity { .. }
            | Change::SetStorageParameters { .. }
            | Change::SetTablePersistence { .. }
            | Change::SetPartitionDefault { .. }
            | Change::SetPartitionNotNull { .. }
            | Change::SetIndexStorageParameters { .. }
            | Change::SetPrimaryKey { .. }
            | Change::AddUnique { .. }
            | Change::DropUnique { .. }
            | Change::AddForeignKey { .. }
            | Change::DropForeignKey { .. }
            | Change::AddCheck { .. }
            | Change::DropCheck { .. }
            | Change::AddIndex { .. }
            | Change::DropIndex { .. }
            | Change::InsertRow { .. }
            | Change::UpdateRow { .. }
            | Change::DeleteRow { .. }
            | Change::SetDataMode { .. }
            | Change::SetEnsureKeys { .. }
            | Change::CreateModule { .. }
            | Change::AlterModule { .. }
            | Change::DropModule { .. }
            | Change::CreateRole { .. }
            | Change::DropRole { .. }
            | Change::RenameRole { .. }
            | Change::Grant { .. }
            | Change::Revoke { .. }
            | Change::PublicExecution { .. } => {}
        }
    }
    types
        .into_iter()
        .filter_map(|ty| ty.type_ref().cloned())
        .collect()
}

async fn read(
    conn: &mut Conn,
    within: Within,
    references: &[TypeRef],
) -> Result<Vec<UserType>, DbError> {
    let reads = user_types::read(conn, within, references).await;
    reads
        .into_iter()
        .zip(references)
        .map(|(read, reference)| match read {
            TypeRead::Present(ty) => Ok(*ty),
            other @ (TypeRead::Absent
            | TypeRead::NotDefined
            | TypeRead::Unreadable(_)
            | TypeRead::NotAdmitted(_)) => Err(failed(
                other
                    .refusal(reference)
                    .expect("a non-present type refuses"),
            )),
        })
        .collect()
}

fn pin(ty: &UserType) -> TypePin {
    TypePin {
        schema: ty.reference().schema().to_owned(),
        name: ty.reference().name().to_owned(),
        oid: ty.oid(),
        kind: ty.kind().as_str().to_owned(),
        digest: ty.pin().as_str().to_owned(),
    }
}

pub async fn trusted_roles(conn: &mut Conn, trusted: &BTreeSet<String>) -> Result<(), DbError> {
    let missing =
        crate::input_path::missing_roles(conn, &trusted.iter().cloned().collect::<Vec<_>>())
            .await?;
    if !missing.is_empty() {
        return Err(failed(format!(
            "trusted_roles names roles that do not exist: {}",
            missing.join(", ")
        )));
    }
    Ok(())
}

pub async fn seal(
    conn: &mut Conn,
    schemas: &[&Schema],
    changes: &ChangeSet,
    trusted: &BTreeSet<String>,
) -> Result<Vec<TypePin>, DbError> {
    trusted_roles(conn, trusted).await?;
    let references: Vec<_> = references(schemas, changes).into_iter().collect();
    let types = read(conn, Within::OwnTransaction, &references).await?;
    for schema in schemas {
        for (table, definition) in &schema.tables {
            let check_default = |column: &str, ty: &ColumnType, is_null: bool| {
                let Some(reference) = ty.type_ref() else {
                    return Ok(());
                };
                if is_null
                    && (ty.is_array()
                        || types.iter().any(|ty| {
                            ty.reference() == reference && ty.kind() != user_types::Kind::Domain
                        }))
                {
                    return Err(failed(format!(
                        "column `{table}.{column}` has a NULL default the engine does not keep; declare no default"
                    )));
                }
                Ok(())
            };
            for (column, spec) in &definition.columns {
                check_default(column, &spec.ty, null_default(spec))?;
            }
            // Partitions declare only overrides; their column types live on the parent.
            if let Some(part) = &definition.partition_of
                && let Some(parent) = schema.tables.get(&part.parent)
            {
                for (column, own) in &part.columns {
                    if let Some(spec) = parent.columns.get(column) {
                        check_default(
                            column,
                            &spec.ty,
                            own.default.as_deref().is_some_and(|expr| {
                                crate::rows::unwrapped_with_type(expr)
                                    .0
                                    .eq_ignore_ascii_case("null")
                            }),
                        )?;
                    }
                }
            }
        }
    }
    for ty in &types {
        if let Some(reached) = ty.reaches_managed(|r| {
            schemas.iter().any(|s| {
                s.tables.keys().any(|t| {
                    r.schema.as_deref() == Some(&t.schema) && r.name.as_deref() == Some(&t.name)
                }) || s.modules.keys().any(|m| {
                    let name = m.object_name();
                    r.schema.as_deref() == Some(&name.schema)
                        && r.name.as_deref() == Some(&name.name)
                })
            })
        }) {
            return Err(failed(format!(
                "type `{}` depends on managed {} `{}`; a referenced type cannot depend on an object pbps changes",
                ty.reference(),
                reached.kind,
                reached.identity
            )));
        }
    }
    Ok(types.iter().map(pin).collect())
}

pub async fn check(
    conn: &mut Conn,
    within: Within,
    expected: &[TypePin],
    changes: &ChangeSet,
) -> Result<(), DbError> {
    let references: Vec<_> = expected
        .iter()
        .map(|p| TypeRef::new(&p.schema, &p.name).map_err(|e| failed(e.to_string())))
        .collect::<Result<_, _>>()?;
    for reference in self::references(&[], changes) {
        if !references.contains(&reference) {
            return Err(failed(format!(
                "plan has no type pin for `{reference}`; plan again against the database"
            )));
        }
    }
    let actual: Vec<_> = read(conn, within, &references)
        .await?
        .iter()
        .map(pin)
        .collect();
    for (old, new) in expected.iter().zip(&actual) {
        if old != new {
            return Err(failed(format!(
                "the referenced type `{}.{}` changed since planning; plan again",
                old.schema, old.name
            )));
        }
    }
    Ok(())
}

fn null_default(column: &pbps_model::Column) -> bool {
    matches!(&column.value_source, pbps_model::ValueSource::Default(expr)
        if crate::rows::unwrapped_with_type(expr).0.eq_ignore_ascii_case("null"))
}

/// Checking for rows reads no values through a user-defined type.
pub async fn check_added(
    conn: &mut Conn,
    changes: &ChangeSet,
    ids: &pbps_model::IdsFile,
    stored_ids: &pbps_model::IdsFile,
    trusted: &BTreeSet<String>,
    within: Within,
) -> Result<(), DbError> {
    for planned in &changes.changes {
        let Change::AddColumn { table, column, .. } = &planned.change else {
            continue;
        };
        let Some(reference) = column.ty.type_ref() else {
            continue;
        };
        // A renamed table is still addressed under its stored identity before DDL.
        let uid = ids
            .table_uid(table)
            .ok_or_else(|| failed(format!("plan has no table identity for `{table}`")))?;
        let stored = stored_ids.tables.get(uid);
        let Some(stored) = stored else { continue };
        let sql = format!(
            "SELECT EXISTS (SELECT FROM {}.{} LIMIT 1) AS holds_rows",
            crate::quote(&stored.schema).map_err(|e| failed(e.to_string()))?,
            crate::quote(&stored.name).map_err(|e| failed(e.to_string()))?
        );
        let rows = conn.query(&sql).await?;
        let holds_rows = rows
            .first()
            .ok_or_else(|| failed("the row-presence read returned no row"))?
            .try_get::<bool>("holds_rows")?
            .ok_or_else(|| failed("row presence was NULL"))?;
        if !holds_rows {
            continue;
        }
        let types = read(conn, within, std::slice::from_ref(reference)).await?;
        let own_default = !matches!(column.value_source, pbps_model::ValueSource::None);
        let added =
            crate::input_path::Added::new(&types[0], own_default).array(column.ty.is_array());
        let added = if null_default(column) {
            added.null_fill()
        } else {
            added
        };
        let trusted: Vec<_> = trusted.iter().cloned().collect();
        let answers = crate::input_path::read(conn, within, &trusted, &[added]).await;
        let answer = answers
            .first()
            .ok_or_else(|| failed("the input-path read returned no answer"))?;
        if let Some(why) = answer.refusal(reference) {
            return Err(failed(why));
        }
    }
    Ok(())
}

/// Offline reconstruction asserts prerequisites before it creates any object.
pub fn bootstrap_guard(
    schema: &Schema,
    trusted: &BTreeSet<String>,
) -> Result<Option<String>, pbps_dialect::DialectError> {
    let refs = references(&[schema], &ChangeSet::default());
    if refs.is_empty() && trusted.is_empty() {
        return Ok(None);
    }
    let mut sql = String::from("BEGIN\n");
    for reference in refs {
        let schema = crate::emit::literal(reference.schema());
        let name = crate::emit::literal(reference.name());
        let message = crate::emit::literal(&format!(
            "required type `{reference}` is missing, unusable or not an enum, domain or standalone composite"
        ));
        sql.push_str(&format!("IF NOT EXISTS (SELECT FROM pg_catalog.pg_type t JOIN pg_catalog.pg_namespace n ON n.oid=t.typnamespace LEFT JOIN pg_catalog.pg_class c ON c.oid=t.typrelid WHERE n.nspname={schema} AND t.typname={name} AND t.typisdefined AND (t.typtype IN ('e','d') OR (t.typtype='c' AND c.relkind='c')) AND pg_catalog.has_schema_privilege(n.oid,'USAGE') AND pg_catalog.has_type_privilege(t.oid,'USAGE')) THEN RAISE EXCEPTION '%', {message}; END IF;\n"));
    }
    for role in trusted {
        sql.push_str(&format!("IF NOT EXISTS (SELECT FROM pg_catalog.pg_roles WHERE rolname={}) THEN RAISE EXCEPTION '%', {}; END IF;\n", crate::emit::literal(role), crate::emit::literal(&format!("trusted role `{role}` does not exist"))));
    }
    sql.push_str("END");
    let tag = crate::emit::dollar_tag(&sql);
    Ok(Some(format!("DO {tag}\n{sql}\n{tag};")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bootstrap_guards_only_references_and_choose_a_tag_the_names_cannot_close() {
        let mut schema = Schema::default();
        assert_eq!(bootstrap_guard(&schema, &BTreeSet::new()).unwrap(), None);
        let mut table = pbps_model::Table::default();
        table.columns.insert(
            "v".into(),
            pbps_model::Column::new(ColumnType::reference(
                TypeRef::new("app", "x$pbps$y").unwrap(),
            )),
        );
        schema
            .tables
            .insert(pbps_model::TableName::new("app", "t"), table);
        let sql = bootstrap_guard(&schema, &BTreeSet::new()).unwrap().unwrap();
        assert!(sql.starts_with("DO $pbps1$"), "{sql}");
        assert_eq!(sql.matches("$pbps1$").count(), 2);
        assert!(
            sql.contains("t.typisdefined")
                && sql.contains("has_type_privilege")
                && sql.contains("c.relkind='c'")
        );
        assert_eq!(references(&[&schema], &ChangeSet::default()).len(), 1);
    }
}
