//! Recovery of a concurrent index build's own non-transactional artifact,
//! and of a staged attach's temporary CHECK (#1638).
//!
//! The emitter supplies the target identity. Capture its absence before the
//! statement, then remove only an invalid index on that same table after a
//! failure. Existing objects, valid indexes and objects on another table are
//! preserved. This follows SPEC 7.6's single-deployer assumption; it is not a
//! lock against a separate session replacing objects during recovery (decision 463).

use pbps_db::{Conn, DbError};
use pbps_dialect::{IndexBuild, ScanCheck, ScanCheckStep, Statement};

/// Execute a staged statement and attach its recovery outcome to the original
/// error, so the failure ledger retains both the SQLSTATE and our own account.
pub async fn execute(conn: &mut Conn, statement: &Statement) -> Result<(), DbError> {
    if let Some(check) = statement
        .scan_check
        .as_ref()
        .filter(|c| c.step == ScanCheckStep::Validate)
    {
        let Err(error) = conn.execute(&statement.sql).await else {
            return Ok(());
        };
        let outcome = match remove_unvalidated(conn, check).await {
            Ok(outcome) => outcome,
            // As for an index: the cleanup driver's message stays out of the
            // durable context (DECISIONS 455/456).
            Err(_) => format!(
                "Removing the temporary CHECK `{}` on `{}` failed; it may remain `NOT VALID`, \
                 which every read of the table refuses. `pbps baseline --reason ...` removes it \
                 by name before it records; the original error follows.",
                check.name, check.table
            ),
        };
        return Err(error.context(outcome));
    }
    let Some(build) = statement
        .index_build
        .as_ref()
        .filter(|_| !statement.transactional)
    else {
        return conn.execute(&statement.sql).await;
    };
    let rows = conn.query_with(
        "SELECT t.oid::bigint AS table_oid, \
         EXISTS (SELECT 1 FROM pg_catalog.pg_class c WHERE c.relnamespace=n.oid AND c.relname=$3) AS existed \
         FROM pg_catalog.pg_class t JOIN pg_catalog.pg_namespace n ON n.oid=t.relnamespace \
         WHERE n.nspname=$1 AND t.relname=$2",
        &[build.table.schema.as_str().into(), build.table.name.as_str().into(), build.name.as_str().into()],
    ).await?;
    let row = rows.first().ok_or_else(|| {
        DbError::Refused(format!(
            "cannot prepare index build `{}`: table `{}` is absent",
            build.name, build.table
        ))
    })?;
    let table_oid = row
        .try_get::<i64>("table_oid")?
        .ok_or_else(|| missing("table_oid"))?;
    let existed = row
        .try_get::<bool>("existed")?
        .ok_or_else(|| missing("existed"))?;
    let Err(error) = conn.execute(&statement.sql).await else {
        return Ok(());
    };
    let outcome = if existed {
        format!(
            "Index recovery preserved `{}.{}`: that name existed before the statement.",
            build.table.schema, build.name
        )
    } else {
        match recover(conn, build, table_oid).await {
            Ok(outcome) => outcome,
            // Never interpolate the cleanup driver's message into a durable
            // context: it can name undeclared data (DECISIONS 455/456).
            Err(_) => format!(
                "Recovery of index `{}.{}` failed; the artifact may remain. Inspect it before retrying; the original build error follows.",
                build.table.schema, build.name
            ),
        }
    };
    Err(error.context(outcome))
}

async fn recover(conn: &mut Conn, build: &IndexBuild, table_oid: i64) -> Result<String, DbError> {
    let name = format!("{}.{}", build.table.schema, build.name);
    let rows = conn
        .query_with(
            "SELECT i.indrelid::bigint AS table_oid, i.indisvalid AS valid \
         FROM pg_catalog.pg_class c JOIN pg_catalog.pg_namespace n ON n.oid=c.relnamespace \
         LEFT JOIN pg_catalog.pg_index i ON i.indexrelid=c.oid \
         WHERE n.nspname=$1 AND c.relname=$2",
            &[
                build.table.schema.as_str().into(),
                build.name.as_str().into(),
            ],
        )
        .await?;
    let Some(row) = rows.first() else {
        return Ok(format!("Index recovery found no artifact named `{name}`."));
    };
    if row.try_get::<i64>("table_oid")? != Some(table_oid)
        || row.try_get::<bool>("valid")? != Some(false)
    {
        return Ok(format!(
            "Index recovery preserved `{name}`: it is not an invalid index on the original table. Inspect it before retrying."
        ));
    }
    let sql = crate::emit::drop_failed_index(build)
        .map_err(|error| DbError::Refused(error.to_string()))?;
    conn.execute(&sql).await?;
    Ok(format!(
        "Index recovery removed invalid index `{name}` left by the failed build."
    ))
}

/// Takes out a staged attach's temporary CHECK after its `VALIDATE` failed
/// (#1638, DEC-1638.1). No checkpoint is taken after the `ADD`, because a
/// `NOT VALID` check makes every read of its table refuse, so the database
/// is back where the last checkpoint recorded it once the check is gone.
/// Only a CHECK of that name still `NOT VALID` on that table is removed: one
/// validated, of another kind, or on another table is not this run's.
async fn remove_unvalidated(conn: &mut Conn, check: &ScanCheck) -> Result<String, DbError> {
    let rows = conn
        .query_with(
            "SELECT k.contype::text AS kind, k.convalidated AS validated \
             FROM pg_catalog.pg_constraint k \
             JOIN pg_catalog.pg_class c ON c.oid = k.conrelid \
             JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
             WHERE n.nspname = $1 AND c.relname = $2 AND k.conname = $3",
            &[
                check.table.schema.as_str().into(),
                check.table.name.as_str().into(),
                check.name.as_str().into(),
            ],
        )
        .await?;
    let Some(row) = rows.first() else {
        return Ok(format!(
            "The temporary CHECK `{}` is not on `{}`; nothing was removed.",
            check.name, check.table
        ));
    };
    if row.try_get::<&str>("kind")? != Some("c") || row.try_get::<bool>("validated")? != Some(false)
    {
        return Ok(format!(
            "`{}` on `{}` is not a `NOT VALID` CHECK, so it was left as it is.",
            check.name, check.table
        ));
    }
    let sql =
        crate::emit::drop_scan_check(check).map_err(|error| DbError::Refused(error.to_string()))?;
    conn.execute(&sql).await?;
    Ok(format!(
        "The temporary CHECK `{}` on `{}` was removed; the database is as the last \
         checkpoint recorded it.",
        check.name, check.table
    ))
}

/// Takes out the temporary CHECKs a staged attach left, before a `baseline`
/// reads the database (#1638, DEC-1638.1).
///
/// `tables` are the tables the newest ledger entry names, with their
/// identities; a staged attach's CHECK is named after its table's identity
/// ([`crate::emit::scan_check_name`]), so each is looked for by that exact
/// name on that table, and nothing else is touched. No declared check can
/// hold such a name ([`crate::emit::SCAN_CHECK_PREFIX`]), so one that stands
/// is pbps's, whatever the run that left it recorded before it stopped.
/// Returns what was removed, for the record.
pub async fn remove_scan_checks<'a>(
    conn: &mut Conn,
    tables: impl IntoIterator<Item = (&'a pbps_model::Uid, &'a pbps_model::TableName)>,
) -> Result<Vec<ScanCheck>, DbError> {
    // One read for every table. The pattern only narrows it: what is removed
    // is matched below by the exact name its table's identity gives.
    let rows = conn
        .query_with(
            "SELECT n.nspname, c.relname, k.conname FROM pg_catalog.pg_constraint k \
             JOIN pg_catalog.pg_class c ON c.oid = k.conrelid \
             JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
             WHERE k.contype = 'c' AND starts_with(k.conname::text, $1)",
            &[crate::emit::SCAN_CHECK_PREFIX.into()],
        )
        .await?;
    let mut standing = std::collections::BTreeSet::new();
    for row in &rows {
        let field = |column: &str| -> Result<String, DbError> {
            row.try_get::<&str>(column)?
                .map(str::to_owned)
                .ok_or_else(|| missing(column))
        };
        standing.insert((field("nspname")?, field("relname")?, field("conname")?));
    }
    let mut removed = Vec::new();
    for (uid, table) in tables {
        let check = ScanCheck {
            table: table.clone(),
            name: crate::emit::scan_check_name(uid),
            step: ScanCheckStep::Add,
        };
        if !standing.contains(&(
            check.table.schema.as_str().to_owned(),
            check.table.name.as_str().to_owned(),
            check.name.clone(),
        )) {
            continue;
        }
        let sql = crate::emit::drop_scan_check(&check)
            .map_err(|error| DbError::Refused(error.to_string()))?;
        conn.execute(&sql).await?;
        removed.push(check);
    }
    Ok(removed)
}

fn missing(column: &str) -> DbError {
    DbError::Refused(format!(
        "index recovery could not read catalog field `{column}`"
    ))
}
