//! DML privileges follow the statements a data declaration can produce.

use pbps_db::{Conn, DbError, Param, doctor::DataTables};

use super::{
    Gap, Securable, flag,
    identity::{Identity, Table},
};

pub(super) async fn missing(
    conn: &mut Conn,
    data: &DataTables,
    identities: &Identity<'_>,
) -> Result<Vec<Gap>, DbError> {
    let mut gaps = Vec::new();
    for (table, demand) in data {
        let target = identities.table(table);
        for (permission, needed, columns) in [
            ("SELECT", true, demand.data_columns()),
            ("INSERT", demand.inserts(), demand.insert_columns()),
            ("UPDATE", demand.corrects(), demand.row_columns()),
            ("DELETE", demand.removes(), &[][..]),
        ] {
            if !needed {
                continue;
            }
            let mut answer = None;
            if let Some(current) = target.name() {
                let columns: Option<Vec<_>> = columns
                    .iter()
                    .map(|column| identities.column(table, current, column))
                    .collect();
                // A new or unresolved column cannot borrow an old column's
                // grant. A table grant still covers columns added by the plan.
                let columns = columns.filter(|columns| !columns.is_empty());
                let column_test = if let Some(columns) = &columns {
                    let values = (0..columns.len())
                        .map(|i| format!("(${}::text)", i + 4))
                        .collect::<Vec<_>>()
                        .join(", ");
                    format!(
                        "NOT EXISTS (SELECT 1 FROM (VALUES {values}) AS wanted(column_name)
                         LEFT JOIN pg_catalog.pg_attribute a
                           ON a.attrelid = c.oid AND a.attname = wanted.column_name
                          AND a.attnum > 0 AND NOT a.attisdropped
                         WHERE a.attnum IS NULL OR NOT
                           pg_catalog.has_column_privilege(c.oid, a.attnum, $3))"
                    )
                } else {
                    "false".to_owned()
                };
                let query = format!(
                    "SELECT pg_catalog.has_table_privilege(c.oid, $3) OR ({column_test}) AS held
                       FROM pg_catalog.pg_class c
                       JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
                      WHERE n.nspname = $1 AND c.relname = $2 AND c.relkind = 'r'"
                );
                let mut params = vec![
                    Param::Str(&current.schema),
                    Param::Str(&current.name),
                    Param::Str(permission),
                ];
                params.extend(columns.iter().flatten().map(|c| Param::Str(c)));
                let rows = conn.query_with(&query, &params).await?;
                if let Some(row) = rows.first() {
                    answer = Some(flag(row, "held")?);
                }
            }
            let (held, why, securable) = match answer {
                Some(held) => (
                    held,
                    "the declared data block needs this privilege for its writes and readback",
                    target.name().expect("a catalog answer has a name").clone(),
                ),
                None if matches!(target, Table::Recorded(_)) => (
                    false,
                    "the recorded data table is absent; its permissions cannot be established",
                    target.name().expect("a recorded table has a name").clone(),
                ),
                None => (
                    future_privilege(conn, &table.schema, permission).await?,
                    "the new data table needs this privilege in the deploying role's effective default table ACL for its target schema",
                    table.clone(),
                ),
            };
            if !held {
                gaps.push(Gap {
                    permission,
                    why: why.to_owned(),
                    securable: Securable::Object(securable),
                });
            }
        }
    }
    Ok(gaps)
}

// Creation uses only current_user's defaults. Per-schema entries add to the
// global ACL (or the engine default when absent); they never replace it.
// Ordinary rights are not implicit in ownership after a self-revoke. Measured
// on PostgreSQL 18 and 16, including inherited and PUBLIC ACL recipients.
async fn future_privilege(
    conn: &mut Conn,
    schema: &str,
    permission: &str,
) -> Result<bool, DbError> {
    let rows = conn.query_with(
        "SELECT me.rolsuper OR EXISTS (
           SELECT 1 FROM pg_catalog.aclexplode(
             COALESCE(global.defaclacl, pg_catalog.acldefault('r', me.oid)) ||
             COALESCE(local.defaclacl, '{}'::pg_catalog.aclitem[])) acl
            WHERE acl.privilege_type = $2 AND
             CASE WHEN acl.grantee = 0 THEN true
                  ELSE pg_catalog.pg_has_role(me.oid, acl.grantee, 'USAGE') END
         ) AS held
         FROM pg_catalog.pg_roles me
         LEFT JOIN pg_catalog.pg_namespace n ON n.nspname = $1
         LEFT JOIN pg_catalog.pg_default_acl global
           ON global.defaclrole = me.oid AND global.defaclnamespace = 0 AND global.defaclobjtype = 'r'
         LEFT JOIN pg_catalog.pg_default_acl local
           ON local.defaclrole = me.oid AND local.defaclnamespace = n.oid AND local.defaclobjtype = 'r'
         WHERE me.rolname = current_user",
        &[Param::Str(schema), Param::Str(permission)],
    ).await?;
    flag(
        rows.first().ok_or_else(|| {
            DbError::BadRow("the deploying role has no default ACL answer".into())
        })?,
        "held",
    )
}
