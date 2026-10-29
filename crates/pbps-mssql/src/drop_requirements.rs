//! SQL Server's closed, target-bound row requirements (DEC-2400.2).
use crate::{
    Mssql,
    ident::{literal, quote},
};
use pbps_db::{Conn, Param};
use pbps_dialect::predicate::{Capabilities, Error, parse};
use pbps_model::{ColumnName, DropGuard, TableName, escape::Escaped};
use std::collections::{BTreeMap, BTreeSet};

// One scope for binding and evaluation. sp_executesql restores the caller's
// settings even on error; the apply transaction and its locks remain shared.
const PINS: &str = "SET LANGUAGE us_english; SET DATEFIRST 7; SET DATEFORMAT mdy;
SET ANSI_NULLS ON; SET ANSI_WARNINGS ON; SET ARITHABORT ON; SET ARITHIGNORE OFF;
SET CONCAT_NULL_YIELDS_NULL ON; SET NUMERIC_ROUNDABORT OFF;";
fn pinned(sql: &str) -> String {
    format!(
        "EXEC sys.sp_executesql {};",
        literal(&format!("{PINS}\n{sql}"))
    )
}
fn refused(message: impl Into<String>) -> String {
    Error::new(message).to_string()
}
fn qualified(table: &TableName) -> Result<String, String> {
    Ok(format!(
        "{}.{}",
        quote(&table.schema).map_err(|e| e.to_string())?,
        quote(&table.name).map_err(|e| e.to_string())?
    ))
}

/// An empty policy catalog is evidence only when every policy is visible.
async fn security(conn: &mut Conn, table: &TableName, target: &str) -> Result<(), String> {
    let sql = format!("SELECT CAST(CASE WHEN COALESCE(HAS_PERMS_BY_NAME(DB_NAME(), N'DATABASE', N'VIEW DEFINITION'),0)=1
        AND NOT ({}) THEN 1 ELSE 0 END AS bit) AS visible,
        CAST(CASE WHEN EXISTS (SELECT 1 FROM sys.security_predicates sp
        JOIN sys.security_policies pol ON pol.object_id=sp.object_id
        WHERE sp.target_object_id=OBJECT_ID(@P1,N'U') AND sp.predicate_type=0 AND pol.is_enabled=1)
        THEN 1 ELSE 0 END AS bit) AS filtered", crate::preflight::ROW_SECURITY_DENIED);
    let rows = conn.query_with(&sql, &[Param::Str(target)]).await
        .map_err(|_| refused("row-security catalog could not be read; grant database VIEW DEFINITION and remove effective metadata DENYs"))?;
    let row = rows
        .first()
        .ok_or_else(|| refused("row-security catalog returned no readable evidence"))?;
    if row.try_get::<bool>("visible").ok().flatten() != Some(true) {
        return Err(refused(
            "complete row-security visibility is required; grant database VIEW DEFINITION and remove effective object/schema metadata DENYs",
        ));
    }
    match row.try_get::<bool>("filtered").ok().flatten() {
        Some(false) => Ok(()),
        Some(true) => Err(refused(format!(
            "the row requirement of {} cannot count all rows while an enabled row-level security FILTER policy is active; disable the policy before retrying",
            table.shown()
        ))),
        None => Err(refused(
            "row-security catalog returned unreadable policy evidence",
        )),
    }
}

fn column_safe(
    table: &TableName,
    name: &ColumnName,
    computed: Option<bool>,
    clr: Option<bool>,
) -> Result<(), String> {
    match (computed, clr) {
        (Some(true), _) => Err(refused(format!(
            "column {} of {} is computed; use a stored column for the row requirement",
            name.shown(),
            table.shown()
        ))),
        (_, Some(true)) => Err(refused(format!(
            "column {} of {} has a CLR type; use a non-CLR stored type for the row requirement",
            name.shown(),
            table.shown()
        ))),
        (Some(false), Some(false)) => Ok(()),
        _ => Err(refused(
            "the target column catalog returned unreadable type evidence",
        )),
    }
}

struct Bound {
    sql: String,
    columns: BTreeSet<ColumnName>,
}
async fn bind(conn: &mut Conn, table: &TableName, predicate: &str) -> Result<Bound, String> {
    let mut parsed = parse(predicate, Mssql).map_err(|e| e.to_string())?;
    let candidates = parsed.column_names().map_err(|e| e.to_string())?;
    let target = qualified(table)?;
    security(conn, table, &target).await?;
    let rows = conn
        .query(
            "SELECT CONVERT(int, SERVERPROPERTY('ProductMajorVersion')) AS major,
        CONVERT(int, compatibility_level) AS compatibility FROM sys.databases WHERE name=DB_NAME()",
        )
        .await
        .map_err(|_| refused("SQL Server version and compatibility could not be read"))?;
    let number = |name| {
        rows.first()
            .and_then(|r| r.try_get::<i32>(name).ok().flatten())
            .and_then(|n| u16::try_from(n).ok())
    };
    let capabilities = Capabilities {
        major: number("major").ok_or_else(|| refused("SQL Server major version is unreadable"))?,
        compatibility: number("compatibility")
            .ok_or_else(|| refused("SQL Server compatibility level is unreadable"))?,
    };
    let mut names = BTreeMap::new();
    for name in candidates {
        // sys.columns applies the catalog's comparison, including case-sensitive
        // databases. Only names resolved for this table reach the vetter.
        let rows = conn
            .query_with(
                "SELECT c.name, c.is_computed,
            CAST(CASE WHEN t.is_assembly_type=1 THEN 1 ELSE 0 END AS bit) AS clr
            FROM sys.columns c JOIN sys.types t ON t.user_type_id=c.user_type_id
            WHERE c.object_id=OBJECT_ID(@P1,N'U') AND c.name=@P2",
                &[Param::Str(&target), Param::Str(name.raw())],
            )
            .await
            .map_err(|_| refused("the target column catalog could not be read"))?;
        let row = rows.first().filter(|_| rows.len() == 1).ok_or_else(|| {
            refused(format!(
                "column {} is not visible in target table {}",
                name.shown(),
                table.shown()
            ))
        })?;
        column_safe(
            table,
            &name,
            row.try_get::<bool>("is_computed").ok().flatten(),
            row.try_get::<bool>("clr").ok().flatten(),
        )?;
        let canonical = row
            .try_get::<&str>("name")
            .ok()
            .flatten()
            .ok_or_else(|| refused("the target column catalog returned an unreadable name"))?;
        names.insert(name, ColumnName::new(canonical));
    }
    let vetted = parsed
        .vet(|name| names.get(name).cloned(), capabilities)
        .map_err(|e| e.to_string())?;
    let sql = vetted.sql().map_err(|e| e.to_string())?;
    // Describe binds types and permissions without evaluating table rows. Never
    // feed the recorded text to SQL Server: only the vetted tree renders SQL.
    let query = format!("SELECT CASE WHEN ({sql}) THEN 0 ELSE 1 END AS violation FROM {target}");
    let describe = pinned(&format!(
        "EXEC sys.sp_describe_first_result_set @tsql={}, @params=NULL, @browse_information_mode=0;",
        literal(&query)
    ));
    let rows = conn.query(&describe).await.map_err(|e| {
        refused(format!(
            "the row requirement `{}` of {} does not bind against this table (SQL Server error {})",
            Escaped(predicate),
            table.shown(),
            Escaped(e.server_error_code().as_deref().unwrap_or("unknown"))
        ))
    })?;
    if rows.len() != 1 || rows[0].try_get::<&str>("name").ok().flatten() != Some("violation") {
        return Err(refused(
            "the row requirement returned unreadable binding evidence",
        ));
    }
    Ok(Bound {
        sql,
        columns: names.into_values().collect(),
    })
}

pub async fn vet(
    conn: &mut Conn,
    table: &TableName,
    predicate: &str,
) -> Result<BTreeSet<ColumnName>, String> {
    Ok(bind(conn, table, predicate).await?.columns)
}

/// The hinted count takes the table X lock directly and retains it through the
/// caller's DDL/commit. Binding errors refuse before removal and need no lock.
/// Keeping the hints on the count also defeats stale
/// SNAPSHOT reads; an earlier lock followed by an unhinted count would not.
pub async fn check(conn: &mut Conn, guard: &DropGuard) -> Result<(), String> {
    conn.execute("IF @@TRANCOUNT=0 OR XACT_STATE()<>1 THROW 50000, N'Row requirements need the drop transaction.', 1;")
        .await.map_err(|_| refused("row requirements need an active, committable drop transaction"))?;
    if guard.detach_from.is_some() {
        return Err(refused(
            "SQL Server row requirements do not support partition detach",
        ));
    }
    let target = qualified(&guard.table)?;
    for requirement in guard.requirements.values() {
        let bound = bind(conn, &guard.table, &requirement.predicate).await?;
        let sql = pinned(&format!(
            "SELECT COUNT_BIG(*) AS violations FROM {target} WITH (TABLOCKX,HOLDLOCK)
            WHERE CASE WHEN ({}) THEN 0 ELSE 1 END=1;",
            bound.sql
        ));
        let rows = conn.query(&sql).await.map_err(|e| format!("the row requirement `{}` of {} could not be evaluated (SQL Server error {}); a query or privilege failure never permits removal",
            Escaped(&requirement.predicate), guard.table.shown(), Escaped(e.server_error_code().as_deref().unwrap_or("unknown"))))?;
        pbps_dialect::drop_requirements::check_count(
            rows.first()
                .and_then(|r| r.try_get::<i64>("violations").ok().flatten()),
            guard,
            requirement,
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn computed_clr_and_unreadable_column_evidence_refuse_with_remedies() {
        let table = "dbo.t".parse().unwrap();
        let name = ColumnName::new("a\n\u{1b}\u{202e}");
        let computed = column_safe(&table, &name, Some(true), Some(false)).unwrap_err();
        assert!(computed.contains("is computed; use a stored column"));
        let clr = column_safe(&table, &name, Some(false), Some(true)).unwrap_err();
        assert!(clr.contains("CLR type; use a non-CLR stored type"));
        for message in [&computed, &clr] {
            assert!(
                message.contains("plan-time assertion") && message.contains("CHECK constraint")
            );
            assert!(!message.contains(['\n', '\u{1b}', '\u{202e}']));
        }
        assert!(column_safe(&table, &name, None, Some(false)).is_err());
        assert!(column_safe(&table, &name, Some(false), None).is_err());
        assert!(column_safe(&table, &name, Some(false), Some(false)).is_ok());
    }
}
