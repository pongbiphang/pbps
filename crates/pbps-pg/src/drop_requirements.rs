//! A derived row check and its drop share the caller's transaction (DEC-2400.1).
use pbps_db::{Conn, Param};
use pbps_model::{DropGuard, TableName, escape::Escaped};

fn qualified(table: &TableName) -> Result<String, String> {
    Ok(format!(
        "{}.{}",
        crate::quote(&table.schema).map_err(|e| e.to_string())?,
        crate::quote(&table.name).map_err(|e| e.to_string())?
    ))
}

/// The caller must already have opened the drop's transaction. These locks are
/// deliberately acquired before the vetter's savepoint, so its rollback keeps
/// them held until the DDL and the caller's commit. Never use ONLY here.
pub async fn check(conn: &mut Conn, guard: &DropGuard) -> Result<(), String> {
    for table in guard
        .detach_from
        .iter()
        .chain(std::iter::once(&guard.table))
    {
        conn.execute(&format!(
            "LOCK TABLE {} IN ACCESS EXCLUSIVE MODE",
            qualified(table)?
        ))
        .await
        .map_err(|_| {
            format!(
                "the row requirement of {} could not acquire its drop lock; nothing was removed",
                guard.table.shown()
            )
        })?;
    }
    let table = qualified(&guard.table)?;
    let rows = conn
        .query_with(
            "SELECT pg_catalog.row_security_active($1::pg_catalog.text::pg_catalog.regclass) AS active",
            &[Param::Str(&table)],
        )
        .await
        .map_err(|_| "the row requirement could not read row security".to_owned())?;
    match rows
        .first()
        .and_then(|r| r.try_get::<bool>("active").ok().flatten())
    {
        Some(false) => {}
        Some(true) => {
            return Err(format!(
                "the row requirement of {} cannot count all rows while row security is active",
                guard.table.shown()
            ));
        }
        None => return Err("the row requirement returned unreadable row-security evidence".into()),
    }
    conn.execute("SET LOCAL search_path = ''")
        .await
        .map_err(|_| "the row requirement could not pin its resolution path".to_owned())?;
    for requirement in guard.requirements.values() {
        crate::input_path::vet_requirement(
            conn,
            crate::pins::Within::CallersTransaction,
            &guard.table,
            &requirement.predicate,
        )
        .await?;
        let sql = format!(
            "SELECT count(*) FROM {table} WHERE ({}) IS NOT TRUE",
            requirement.predicate
        );
        let rows = conn.query_with(&sql, &[]).await.map_err(|e| format!(
            "the row requirement `{}` of {} could not be evaluated (SQLSTATE {}); a query or privilege failure never permits removal",
            Escaped(&requirement.predicate), guard.table.shown(), e.server_error_code().as_deref().unwrap_or("unknown")))?;
        let count = rows
            .first()
            .and_then(|r| r.try_get_at::<i64>(0).ok().flatten())
            .filter(|n| *n >= 0)
            .ok_or("the row requirement returned no readable count; nothing was removed")?;
        if count > 0 {
            return Err(format!(
                "{count} row(s) of {} violate the row requirement `{}`; nothing was removed",
                guard.table.shown(),
                Escaped(&requirement.predicate)
            ));
        }
    }
    Ok(())
}
