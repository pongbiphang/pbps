//! Absence needs a readable DENY inventory over the lookup path (DEC-2541.3).
use super::{CaptureError, ObjectIdentity, Schema, Uncovered};
use pbps_db::{Row, transport::QueryConnection};
use std::collections::BTreeSet;

const AUTHORITY: &str = "SELECT CONVERT(nvarchar(128), USER_NAME()) AS principal, \
    CONVERT(nvarchar(128), DB_NAME()) AS database_name, \
    CONVERT(nvarchar(1), ISNULL(HAS_PERMS_BY_NAME(NULL, N'DATABASE', N'VIEW SECURITY DEFINITION'), 0)) AS readable;";

fn value(row: &Row, name: &str) -> Result<Option<String>, CaptureError> {
    row.try_get::<&str>(name)
        .map(|s| s.map(str::to_owned))
        .map_err(|_| CaptureError::Incomplete)
}
fn required(row: &Row, name: &str) -> Result<String, CaptureError> {
    value(row, name)?.ok_or(CaptureError::Incomplete)
}
fn named(parts: Vec<String>, condition: &'static str) -> CaptureError {
    CaptureError::Coverage(Uncovered::object(
        &ObjectIdentity {
            class: "sys.database_permissions".into(),
            name: parts,
            signature: Vec::new(),
        },
        condition,
    ))
}
fn denies(schemas: &[Schema]) -> String {
    let names: BTreeSet<_> = schemas
        .iter()
        .flat_map(|schema| {
            schema
                .tables
                .keys()
                .map(|id| id.schema.as_str())
                .chain(schema.modules.keys().map(|id| id.schema()))
                .chain(schema.types.keys().map(|id| id.schema()))
        })
        .collect();
    let mut path = vec!["(SCHEMA_NAME())".to_owned(), "(N'dbo')".to_owned()];
    path.extend(
        names
            .into_iter()
            .map(|name| format!("({})", crate::ident::literal(name))),
    );
    // Compare names under the database collation; Rust equality cannot prove
    // that a case variant or a delimited identifier is outside the path.
    format!(
        "SELECT CONVERT(nvarchar(128), t.name) AS grantee, \
        CONVERT(nvarchar(128), p.permission_name) AS permission, \
        CONVERT(nvarchar(20), p.major_id) AS major_id, \
        CONVERT(nvarchar(20), p.class) AS class, \
        CONVERT(nvarchar(128), CASE p.class WHEN 0 THEN DB_NAME() \
            WHEN 1 THEN OBJECT_SCHEMA_NAME(p.major_id) WHEN 3 THEN SCHEMA_NAME(p.major_id) END) AS securable, \
        CONVERT(nvarchar(128), CASE WHEN p.class=1 THEN OBJECT_NAME(p.major_id) END) AS object_name \
        FROM sys.database_permissions p JOIN sys.user_token t ON t.principal_id=p.grantee_principal_id \
        WHERE p.state=N'D' AND p.class IN (0,1,3) \
        AND p.permission_name IN (N'VIEW DEFINITION',N'VIEW PERFORMANCE DEFINITION',N'CONTROL') \
        AND (p.class=0 OR \
            (p.class=1 AND (OBJECT_SCHEMA_NAME(p.major_id) IS NULL OR EXISTS \
                (SELECT 1 FROM (VALUES {path}) q(name) WHERE q.name=OBJECT_SCHEMA_NAME(p.major_id)))) OR \
            (p.class=3 AND (SCHEMA_NAME(p.major_id) IS NULL OR EXISTS \
                (SELECT 1 FROM (VALUES {path}) q(name) WHERE q.name=SCHEMA_NAME(p.major_id))))) \
        ORDER BY p.class,p.major_id,p.grantee_principal_id,p.permission_name;",
        path = path.join(",")
    )
}

pub(in crate::resolver::capture::staged) async fn prove(
    connection: &mut impl QueryConnection,
    schemas: &[Schema],
) -> Result<(), CaptureError> {
    let rows = connection
        .query(AUTHORITY)
        .await
        .map_err(|_| CaptureError::Read)?;
    let [row] = rows.as_slice() else {
        return Err(CaptureError::Incomplete);
    };
    // Security-definition DENY can hide the direct DENY rows themselves,
    // even when database VIEW DEFINITION remains effective.
    match required(row, "readable")?.as_str() {
        "1" => {}
        "0" => {
            return Err(named(
                vec![
                    format!("principal: {}", required(row, "principal")?),
                    format!("database: {}", required(row, "database_name")?),
                ],
                "absence proof requires effective database VIEW SECURITY DEFINITION to read the DENY list; lift the DENY for the pbps principal only, or use a separate catalog-reader account",
            ));
        }
        _ => return Err(CaptureError::Incomplete),
    }
    let rows = connection.query(&denies(schemas)).await.map_err(|_| {
        CaptureError::Coverage(Uncovered::class("sys.database_permissions / sys.user_token",
            "the absence proof could not read the DENY inventory; use a separate catalog-reader account"))
    })?;
    if let Some(row) = rows.first() {
        let class = required(row, "class")?;
        let major = required(row, "major_id")?;
        let scope = match class.as_str() {
            "0" => "database",
            "1" => "object",
            "3" => "schema",
            _ => return Err(CaptureError::Incomplete),
        };
        let mut parts = vec![
            format!("grantee: {}", required(row, "grantee")?),
            format!("DENY {}", required(row, "permission")?),
            format!("scope: {scope}"),
        ];
        match value(row, "securable")? {
            Some(name) => {
                parts.push(name);
                if class == "1" {
                    parts.push(
                        value(row, "object_name")?
                            .unwrap_or_else(|| format!("hidden major_id={major}")),
                    );
                }
            }
            None => parts.push(format!("hidden major_id={major}")),
        }
        return Err(named(
            parts,
            "metadata-hiding DENY prevents proving absence on the name-resolution path; lift the DENY for the pbps principal only, or use a separate catalog-reader account",
        ));
    }
    Ok(())
}
