//! DML privileges follow the statements a data declaration can produce.

use pbps_db::{Conn, DbError, Param, doctor::DataTables};

use super::{Gap, Securable, flag};

pub(super) async fn missing(conn: &mut Conn, data: &DataTables) -> Result<Vec<Gap>, DbError> {
    let mut gaps = Vec::new();
    for (table, demand) in data {
        for (permission, needed, columns) in [
            ("SELECT", true, demand.data_columns()),
            ("INSERT", demand.inserts(), demand.data_columns()),
            ("UPDATE", demand.corrects(), demand.row_columns()),
            ("DELETE", demand.removes(), &[][..]),
        ] {
            if !needed {
                continue;
            }
            // A column grant suffices only when it covers every column the
            // emitted statement names. DELETE has no column-level form.
            let column_test = if columns.is_empty() {
                "false".to_owned()
            } else {
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
            };
            let query = format!(
                "SELECT pg_catalog.has_table_privilege(c.oid, $3) OR ({column_test}) AS held
                   FROM pg_catalog.pg_class c
                   JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
                  WHERE n.nspname = $1 AND c.relname = $2 AND c.relkind = 'r'"
            );
            let mut params = vec![
                Param::Str(&table.schema),
                Param::Str(&table.name),
                Param::Str(permission),
            ];
            params.extend(columns.iter().map(|c| Param::Str(c)));
            for row in conn.query_with(&query, &params).await? {
                if !flag(&row, "held")? {
                    gaps.push(Gap {
                        permission,
                        why: "the declared data block needs this privilege for its writes and readback",
                        securable: Securable::Object(table.clone()),
                    });
                }
            }
        }
    }
    Ok(gaps)
}
