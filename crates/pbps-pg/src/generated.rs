//! The live facts a plan over generated columns needs and the model does not
//! hold (DEC-1168.1): which server version this is, and which columns a
//! generated column is computed from.
//!
//! Neither is a property of a declaration. `SET EXPRESSION` exists from
//! PostgreSQL 17, and which columns an expression reads is the engine's own
//! record in `pg_depend` — asked of the catalog rather than parsed out of the
//! expression's text, which would be a second, weaker SQL parser.

use pbps_db::{Conn, DbError};
use pbps_model::TableName;

/// The first release with `ALTER COLUMN ... SET EXPRESSION`. Measured: a
/// syntax error on 16.15, accepted on 17.11 and 18.6.
pub const SET_EXPRESSION_ARRIVED_IN: i64 = 170_000;

/// One generated column's dependence on another column of its table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dependence {
    /// The column the expression reads.
    pub base: String,
    /// The generated column computed from it.
    pub generated: String,
}

/// Every column of `table` that a generated column of the same table is
/// computed from, as the engine recorded it when the expression was created:
/// `pg_depend` from the expression's `pg_attrdef` row to each column it reads.
/// Measured on all three supported releases, the engine refuses to retype
/// such a column ("cannot alter type of a column used by a generated
/// column") and drops it only with `CASCADE`.
///
/// Not the expression's edge to the column it belongs to: measured on 16.15,
/// 17.11 and 18.6, `pg_attrdef` also depends on its own column (`deptype`
/// `i`), and that column retypes freely. A generated column never reads
/// another generated column, let alone itself, so no input is lost.
///
/// Empty where the table has no generated column or does not exist yet, which
/// is the same answer here: nothing live depends on anything.
pub async fn dependences(conn: &mut Conn, table: &TableName) -> Result<Vec<Dependence>, DbError> {
    let sql = format!(
        "SELECT base.attname AS base, gen.attname AS generated
           FROM pg_catalog.pg_attrdef ad
           JOIN pg_catalog.pg_attribute gen
             ON gen.attrelid = ad.adrelid AND gen.attnum = ad.adnum AND gen.attgenerated <> ''
           JOIN pg_catalog.pg_depend d
             ON d.classid = 'pg_catalog.pg_attrdef'::pg_catalog.regclass AND d.objid = ad.oid
            AND d.refclassid = 'pg_catalog.pg_class'::pg_catalog.regclass
            AND d.refobjid = ad.adrelid AND d.refobjsubid > 0
            AND d.refobjsubid <> ad.adnum
           JOIN pg_catalog.pg_attribute base
             ON base.attrelid = d.refobjid AND base.attnum = d.refobjsubid
           JOIN pg_catalog.pg_class c ON c.oid = ad.adrelid
           JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
          WHERE n.nspname = {} AND c.relname = {}
          ORDER BY base.attname, gen.attname",
        literal(&table.schema),
        literal(&table.name)
    );
    let mut out = Vec::new();
    for row in conn.query(&sql).await? {
        let text = |column: &str| -> Result<String, DbError> {
            row.try_get::<&str>(column)?
                .map(str::to_owned)
                .ok_or_else(|| DbError::BadRow(format!("catalog column `{column}` is null")))
        };
        out.push(Dependence {
            base: text("base")?,
            generated: text("generated")?,
        });
    }
    Ok(out)
}

fn literal(value: &str) -> String {
    format!("E'{}'", value.replace('\\', "\\\\").replace('\'', "''"))
}
