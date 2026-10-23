//! Compare declared cross-database shapes without adding managed objects.

use pbps_db::{Conn, DbError, Row};
use pbps_model::dependency::{
    Comparison, Dependencies, DependencyColumn, DependencyKind, PlannedDependencies,
    PlannedDependency,
};
use std::collections::BTreeMap;

fn required<'a, T: pbps_db::FromColumn<'a>>(row: &'a Row, column: &str) -> Result<T, DbError> {
    row.try_get(column)?
        .ok_or_else(|| DbError::Refused(format!("dependency catalog returned NULL for {column}")))
}

fn in_database(database: &str, query: &str) -> Result<String, DbError> {
    let database = crate::ident::quote(database).map_err(|e| DbError::Refused(e.to_string()))?;
    Ok(format!(
        "EXEC ({})",
        crate::ident::literal(&format!("USE {database}; {query}"))
    ))
}

pub async fn compare(
    conn: &mut Conn,
    declarations: &Dependencies,
) -> Result<PlannedDependencies, DbError> {
    let mut result = BTreeMap::new();
    for (name, declaration) in declarations {
        let refuse =
            |why: String| DbError::Refused(format!("dependency `{}`: {why}", name.shown()));
        let database = crate::ident::literal(name.database());
        let rows = conn.query(&format!(
            "SELECT DB_ID({database}) AS db_id, DB_ID() AS target_id, HAS_DBACCESS({database}) AS access;"
        )).await?;
        let row = rows
            .first()
            .ok_or_else(|| refuse("database identity was unreadable".into()))?;
        let mut db_id = row.try_get::<i32>("db_id")?;
        let target_id = required::<i32>(row, "target_id")?;
        let mut access = row.try_get::<i32>("access")?;
        // DENY VIEW ANY DATABASE hides DB_ID even though HAS_DBACCESS=0
        // proves the inaccessible database exists. NULL/NULL is not absence:
        // USE distinguishes a missing database (911) from denied access (916).
        // Other failures keep their source and never become an assumption.
        if db_id.is_none() && access != Some(0) {
            let probe = in_database(name.database(), "SELECT DB_ID() AS db_id;")?;
            match conn.query(&probe).await {
                Ok(rows) => {
                    let row = rows
                        .first()
                        .ok_or_else(|| refuse("database existence probe returned no row".into()))?;
                    db_id = Some(required::<i32>(row, "db_id")?);
                    access = Some(1);
                }
                Err(error) => match error.server_error_code().as_deref() {
                    Some("911") => return Err(refuse("database is missing".into())),
                    Some("916") => access = Some(0),
                    _ => {
                        return Err(error.context(format!(
                            "cannot inspect dependency `{}` database",
                            name.shown()
                        )));
                    }
                },
            }
        }
        if db_id == Some(target_id) {
            return Err(refuse("the database is the planning connection's target; manage the object or use an external".into()));
        }
        let mut comparison = Comparison::NotCompared;
        match access {
            Some(0) => {} // HAS_DBACCESS is positive proof; an empty catalog is not.
            Some(1) => {
                let object = name.object();
                let schema = crate::ident::literal(&object.schema);
                let local = crate::ident::literal(&object.name);
                let qualified = format!(
                    "{} + N'.' + {}",
                    crate::ident::literal(
                        &crate::ident::quote(&object.schema).map_err(|e| refuse(e.to_string()))?
                    ),
                    crate::ident::literal(
                        &crate::ident::quote(&object.name).map_err(|e| refuse(e.to_string()))?
                    )
                );
                // DEC-1192.1: a hidden object and a missing object both have
                // OBJECT_ID NULL and HAS_PERMS_BY_NAME 0. Effective DENY rows
                // remain visible, even when their owner's name is hidden.
                let visibility = format!(
                    r#"
SELECT o.type AS kind,
 CONVERT(int, CASE WHEN HAS_PERMS_BY_NAME({qualified}, 'OBJECT', 'VIEW DEFINITION') = 1 THEN 1
 WHEN HAS_PERMS_BY_NAME(DB_NAME(), 'DATABASE', 'VIEW DEFINITION') = 1
  AND NOT EXISTS (SELECT 1 FROM sys.schemas s WHERE s.name = {schema} COLLATE CATALOG_DEFAULT
   AND COALESCE(HAS_PERMS_BY_NAME(QUOTENAME(s.name), 'SCHEMA', 'VIEW DEFINITION'), 0) <> 1)
  AND NOT EXISTS (SELECT 1 FROM sys.database_permissions dp
   WHERE dp.class = 1 AND dp.state = N'D' AND dp.permission_name IN (N'VIEW DEFINITION', N'CONTROL')
    AND (dp.grantee_principal_id = USER_ID() OR IS_MEMBER(USER_NAME(dp.grantee_principal_id)) = 1)
    AND COALESCE(HAS_PERMS_BY_NAME(QUOTENAME(OBJECT_SCHEMA_NAME(dp.major_id))+N'.'+QUOTENAME(OBJECT_NAME(dp.major_id)), 'OBJECT', 'VIEW DEFINITION'), 0) <> 1)
 THEN 1 ELSE 0 END) AS readable
FROM (VALUES (1)) AS seed(n)
LEFT JOIN (sys.objects o JOIN sys.schemas s ON s.schema_id = o.schema_id)
 ON s.name = {schema} COLLATE CATALOG_DEFAULT AND o.name = {local} COLLATE CATALOG_DEFAULT;
"#
                );
                let rows = conn
                    .query(&in_database(name.database(), &visibility)?)
                    .await?;
                let row = rows
                    .first()
                    .ok_or_else(|| refuse("catalog visibility proof returned no row".into()))?;
                if required::<i32>(row, "readable")? == 1 {
                    let kind = row.try_get::<&str>("kind")?.map(str::trim);
                    let expected = match declaration.kind {
                        DependencyKind::Table => "U",
                        DependencyKind::View => "V",
                    };
                    if kind.is_none() {
                        return Err(refuse("object is missing in a readable catalog".into()));
                    }
                    if kind != Some(expected) {
                        return Err(refuse(format!(
                            "kind differs: declared {}, catalog {}",
                            declaration.kind.as_str(),
                            kind.unwrap()
                        )));
                    }
                    let columns = format!(
                        r#"
SELECT c.name, ty.name AS type_name, c.max_length, c.precision, c.scale, c.is_nullable
FROM sys.objects o JOIN sys.schemas s ON s.schema_id = o.schema_id
JOIN sys.columns c ON c.object_id = o.object_id
JOIN sys.types ty ON ty.user_type_id = c.user_type_id
WHERE s.name = {schema} COLLATE CATALOG_DEFAULT AND o.name = {local} COLLATE CATALOG_DEFAULT;
"#
                    );
                    let rows = conn.query(&in_database(name.database(), &columns)?).await?;
                    let mut actual: BTreeMap<pbps_model::ColumnName, DependencyColumn> =
                        BTreeMap::new();
                    for row in &rows {
                        let column = required::<&str>(row, "name")?;
                        let ty = crate::introspect::catalog_column_type(
                            required(row, "type_name")?,
                            required(row, "max_length")?,
                            required(row, "precision")?,
                            required(row, "scale")?,
                        )
                        .map_err(|e| {
                            refuse(format!(
                                "column `{}`: {e}",
                                pbps_model::escape::Escaped(column)
                            ))
                        })?;
                        actual.insert(
                            column.into(),
                            DependencyColumn {
                                ty,
                                nullable: required(row, "is_nullable")?,
                            },
                        );
                    }
                    compare_columns(declaration, &actual).map_err(refuse)?;
                    comparison = Comparison::Compared;
                }
            }
            _ => {
                return Err(refuse(
                    "database accessibility was unreadable; it is not an empty catalog".into(),
                ));
            }
        }
        result.insert(
            name.clone(),
            PlannedDependency {
                declaration: declaration.clone(),
                comparison,
            },
        );
    }
    Ok(result)
}

fn compare_columns(
    declaration: &pbps_model::dependency::Dependency,
    actual: &BTreeMap<pbps_model::ColumnName, DependencyColumn>,
) -> Result<(), String> {
    for (column, declared) in &declaration.columns {
        let Some(found) = actual.get(column) else {
            return Err(format!("column `{}` is missing", column.shown()));
        };
        let ty = crate::types::normalize(&declared.ty).map_err(|e| e.to_string())?;
        if ty != found.ty {
            return Err(format!(
                "column `{}` type differs: declared {}, catalog {}",
                column.shown(),
                ty.shown(),
                found.ty.shown()
            ));
        }
        if declared.nullable != found.nullable {
            return Err(format!(
                "column `{}` nullability differs: declared {}, catalog {}",
                column.shown(),
                declared.nullable,
                found.nullable
            ));
        }
    }

    for column in actual.keys() {
        if !declaration.columns.contains_key(column) {
            return Err(format!("extra catalog column `{}`", column.shown()));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use pbps_model::dependency::Dependency;

    fn declaration() -> Dependency {
        Dependency {
            kind: DependencyKind::Table,
            columns: [
                (
                    "id".into(),
                    DependencyColumn {
                        ty: "integer".parse().unwrap(),
                        nullable: false,
                    },
                ),
                (
                    "name".into(),
                    DependencyColumn {
                        ty: "nvarchar(100)".parse().unwrap(),
                        nullable: true,
                    },
                ),
            ]
            .into(),
        }
    }

    #[test]
    fn native_types_nullable_flags_and_exact_column_sets_are_compared() {
        let expected = declaration();
        let mut actual = expected.columns.clone();
        actual.get_mut("id").unwrap().ty = "int".parse().unwrap();
        assert_eq!(compare_columns(&expected, &actual), Ok(()));
        for (name, ty, nullable, difference) in [
            ("id", "bigint", false, "type differs"),
            ("id", "int", true, "nullability differs"),
            ("name", "nvarchar(99)", true, "type differs"),
        ] {
            let mut changed = actual.clone();
            changed.insert(
                name.into(),
                DependencyColumn {
                    ty: ty.parse().unwrap(),
                    nullable,
                },
            );
            let message = compare_columns(&expected, &changed).unwrap_err();
            assert!(
                message.contains(name) && message.contains(difference),
                "{message}"
            );
        }
        let mut missing = actual.clone();
        missing.remove("name");
        assert!(
            compare_columns(&expected, &missing)
                .unwrap_err()
                .contains("`name` is missing")
        );
        let mut extra = actual.clone();
        extra.insert("extra".into(), actual["id"].clone());
        assert!(
            compare_columns(&expected, &extra)
                .unwrap_err()
                .contains("extra catalog column `extra`")
        );
        assert!(compare_columns(&expected, &Default::default()).is_err());
    }
}
