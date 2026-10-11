//! What pg_partman says about the partitioned tables it maintains (#1533,
//! DEC-1533.1).
//!
//! The catalog cannot say who created a partition. pg_partman can: every
//! parent it maintains has a row in its `part_config`, in the schema the
//! extension was installed into. `pull` reads it to declare those parents'
//! partitions `external`, and leaves pg_partman's template tables out, which
//! it creates beside its own catalog but outside the extension.

use pbps_db::{Conn, DbError};
use pbps_model::TableName;

/// One parent pg_partman maintains, with its template table where it has one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Maintained {
    pub parent: TableName,
    pub template: Option<TableName>,
}

/// The schema pg_partman is installed in, or `None` where it is not.
pub async fn schema(conn: &mut Conn) -> Result<Option<String>, DbError> {
    let rows = conn
        .query(
            "SELECT n.nspname::text AS schema FROM pg_catalog.pg_extension e \
             JOIN pg_catalog.pg_namespace n ON n.oid = e.extnamespace \
             WHERE e.extname = 'pg_partman'",
        )
        .await?;
    rows.first()
        .map(|row| {
            row.try_get::<&str>("schema")?
                .map(str::to_owned)
                .ok_or_else(|| DbError::BadRow("catalog column `schema` is null".into()))
        })
        .transpose()
}

/// Every parent pg_partman's `part_config` in `schema` names that exists,
/// resolved through the catalog: its text is the name pg_partman was given,
/// schema-qualified, and a row whose table is gone resolves to nothing.
///
/// An error here is pg_partman's catalog unreadable, measured on 18 as
/// "permission denied for schema partman" for a role without `USAGE`, and is
/// returned as one: it is not "no parent is maintained".
pub async fn maintained(conn: &mut Conn, schema: &str) -> Result<Vec<Maintained>, DbError> {
    let config = format!(
        "{}.part_config",
        crate::quote(schema).map_err(|e| DbError::BadRow(e.to_string()))?
    );
    let rows = conn
        .query(&format!(
            "SELECT pn.nspname::text AS parent_schema, pc.relname::text AS parent, \
                    tn.nspname::text AS template_schema, tc.relname::text AS template \
             FROM {config} AS p \
             JOIN pg_catalog.pg_class pc ON pc.oid = pg_catalog.to_regclass(p.parent_table) \
             JOIN pg_catalog.pg_namespace pn ON pn.oid = pc.relnamespace \
             LEFT JOIN pg_catalog.pg_class tc \
               ON tc.oid = pg_catalog.to_regclass(p.template_table) \
             LEFT JOIN pg_catalog.pg_namespace tn ON tn.oid = tc.relnamespace \
             ORDER BY 1, 2"
        ))
        .await?;
    let mut out = Vec::new();
    for row in rows {
        let text = |column: &str| -> Result<Option<String>, DbError> {
            Ok(row.try_get::<&str>(column)?.map(str::to_owned))
        };
        let (Some(parent_schema), Some(parent)) = (text("parent_schema")?, text("parent")?) else {
            return Err(DbError::BadRow(
                "pg_partman's parent resolved without a name".into(),
            ));
        };
        let template = match (text("template_schema")?, text("template")?) {
            (Some(s), Some(t)) => Some(TableName::new(s, t)),
            _ => None,
        };
        out.push(Maintained {
            parent: TableName::new(parent_schema, parent),
            template,
        });
    }
    Ok(out)
}
