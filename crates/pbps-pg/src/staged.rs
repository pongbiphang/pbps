//! Recovery of a concurrent index build's own non-transactional artifact.
//!
//! The emitter supplies the target identity. Capture its absence before the
//! statement, then remove only an invalid index on that same table after a
//! failure. Existing objects, valid indexes and objects on another table are
//! preserved. This follows SPEC 7.6's single-deployer assumption; it is not a
//! lock against a separate session replacing objects during recovery (decision 463).

use pbps_db::{Conn, DbError};
use pbps_dialect::{IndexBuild, Statement};

/// Execute a staged statement and attach its recovery outcome to the original
/// error, so the failure ledger retains both the SQLSTATE and our own account.
pub async fn execute(conn: &mut Conn, statement: &Statement) -> Result<(), DbError> {
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

fn missing(column: &str) -> DbError {
    DbError::Refused(format!(
        "index recovery could not read catalog field `{column}`"
    ))
}
