//! The catalog queries behind [`crate::introspect`].
//!
//! Only this file runs SQL against a live server; it converts `sys.*` rows into
//! the plain `Raw*` structs and hands them to the pure assembler. The queries
//! are static — nothing user-controlled is ever interpolated into them.

use pbps_db::{Conn, DbError, Row};

use crate::introspect::{
    Pulled, RawCatalog, RawCheck, RawColumn, RawForeignKeyColumn, RawIndexColumn, RawKeyColumn,
    RawModule, RawTable, assemble,
};

/// `is_ms_shipped = 0` drops the system tables; the `__pbps_` filter drops this
/// tool's own state and lock tables — they must never enter the managed set, or
/// the tool would plan changes to itself.
const TABLES: &str = "\
SELECT t.object_id, s.name AS schema_name, t.name AS table_name
  FROM sys.tables t
  JOIN sys.schemas s ON s.schema_id = t.schema_id
 WHERE t.is_ms_shipped = 0
   AND t.name NOT LIKE '\\_\\_pbps\\_%' ESCAPE '\\'
 ORDER BY s.name, t.name;";

const COLUMNS: &str = "\
SELECT c.object_id, c.name, ty.name AS type_name,
       c.max_length, c.precision, c.scale, c.is_nullable, c.is_computed,
       CONVERT(bit, CASE WHEN ty.is_user_defined = 1 THEN 1 ELSE 0 END) AS is_udt,
       CONVERT(bigint, ic.seed_value) AS seed,
       CONVERT(bigint, ic.increment_value) AS increment,
       dc.definition AS default_definition
  FROM sys.columns c
  JOIN sys.types ty ON ty.user_type_id = c.user_type_id
  LEFT JOIN sys.identity_columns ic
         ON ic.object_id = c.object_id AND ic.column_id = c.column_id
  LEFT JOIN sys.default_constraints dc
         ON dc.parent_object_id = c.object_id AND dc.parent_column_id = c.column_id
 ORDER BY c.object_id, c.column_id;";

const KEY_COLUMNS: &str = "\
SELECT kc.parent_object_id AS object_id, kc.name,
       CONVERT(bit, CASE WHEN kc.type = 'PK' THEN 1 ELSE 0 END) AS is_primary,
       col.name AS column_name
  FROM sys.key_constraints kc
  JOIN sys.index_columns ic
    ON ic.object_id = kc.parent_object_id AND ic.index_id = kc.unique_index_id
  JOIN sys.columns col
    ON col.object_id = ic.object_id AND col.column_id = ic.column_id
 WHERE ic.is_included_column = 0
 ORDER BY kc.parent_object_id, kc.name, ic.key_ordinal;";

const FOREIGN_KEY_COLUMNS: &str = "\
SELECT fk.parent_object_id AS object_id, fk.name,
       rs.name AS ref_schema, rt.name AS ref_table,
       pc.name AS column_name, rc.name AS ref_column_name,
       fk.delete_referential_action, fk.update_referential_action
  FROM sys.foreign_keys fk
  JOIN sys.foreign_key_columns fkc ON fkc.constraint_object_id = fk.object_id
  JOIN sys.columns pc
    ON pc.object_id = fkc.parent_object_id AND pc.column_id = fkc.parent_column_id
  JOIN sys.columns rc
    ON rc.object_id = fkc.referenced_object_id AND rc.column_id = fkc.referenced_column_id
  JOIN sys.tables rt ON rt.object_id = fk.referenced_object_id
  JOIN sys.schemas rs ON rs.schema_id = rt.schema_id
 ORDER BY fk.parent_object_id, fk.name, fkc.constraint_column_id;";

const CHECKS: &str = "\
SELECT cc.parent_object_id AS object_id, cc.name, cc.definition
  FROM sys.check_constraints cc
 WHERE cc.is_ms_shipped = 0
 ORDER BY cc.parent_object_id, cc.name;";

/// PK- and UNIQUE-backing indexes are already covered by `KEY_COLUMNS`;
/// hypothetical indexes are the tuning wizard's ghosts. Included columns sort
/// after key columns so the assembler sees keys in key order first.
const INDEX_COLUMNS: &str = "\
SELECT i.object_id, i.name, i.is_unique,
       CONVERT(bit, CASE WHEN i.type = 1 THEN 1 ELSE 0 END) AS is_clustered,
       i.filter_definition,
       col.name AS column_name, ic.is_included_column, ic.is_descending_key
  FROM sys.indexes i
  JOIN sys.index_columns ic
    ON ic.object_id = i.object_id AND ic.index_id = i.index_id
  JOIN sys.columns col
    ON col.object_id = ic.object_id AND col.column_id = ic.column_id
 WHERE i.type > 0 AND i.is_primary_key = 0 AND i.is_unique_constraint = 0
   AND i.is_hypothetical = 0
 ORDER BY i.object_id, i.name, ic.is_included_column, ic.key_ordinal;";

/// Views, procedures, functions and triggers (ADR-0002).
///
/// `sys.sql_modules` stores the definition **verbatim** — SQL Server does not
/// rewrite it the way it rewrites a check constraint's expression — which is
/// what makes the round trip near-exact and the drift false-positive risk lower
/// here than for constraints. The join is a LEFT one because two kinds of
/// module have no readable definition: a CLR object, and one created WITH
/// ENCRYPTION. Both come back NULL and are reported as unmanageable rather than
/// silently skipped.
///
/// `parent_object_id` gives a trigger its table. `is_ms_shipped = 0` drops the
/// system objects.
const MODULES: &str = "\
SELECT s.name AS schema_name, o.name AS object_name, o.type AS type_code,
       m.definition AS definition,
       ps.name AS parent_schema, pt.name AS parent_table
  FROM sys.objects o
  JOIN sys.schemas s ON s.schema_id = o.schema_id
  LEFT JOIN sys.sql_modules m ON m.object_id = o.object_id
  LEFT JOIN sys.tables pt ON pt.object_id = o.parent_object_id
  LEFT JOIN sys.schemas ps ON ps.schema_id = pt.schema_id
 WHERE o.is_ms_shipped = 0
   AND o.type IN ('V', 'P', 'PC', 'FN', 'IF', 'TF', 'FS', 'FT', 'TR')
 ORDER BY s.name, o.name;";

/// A required value that came back NULL means the query and the struct have
/// drifted apart; that is a bug here, not bad data, and it must be named.
pub(crate) fn get<'a, T: tiberius::FromSql<'a>>(row: &'a Row, col: &str) -> Result<T, DbError> {
    row.try_get::<T, _>(col)?
        .ok_or_else(|| DbError::BadRow(format!("column `{col}` is unexpectedly NULL")))
}

pub(crate) fn opt<'a, T: tiberius::FromSql<'a>>(
    row: &'a Row,
    col: &str,
) -> Result<Option<T>, DbError> {
    Ok(row.try_get::<T, _>(col)?)
}

/// Reads the whole catalog and assembles it into a schema.
pub async fn introspect(conn: &mut Conn) -> Result<Pulled, DbError> {
    let mut raw = RawCatalog::default();

    for row in conn.query(TABLES).await? {
        raw.tables.push(RawTable {
            object_id: get(&row, "object_id")?,
            schema: get::<&str>(&row, "schema_name")?.to_owned(),
            name: get::<&str>(&row, "table_name")?.to_owned(),
        });
    }

    for row in conn.query(COLUMNS).await? {
        let seed: Option<i64> = opt(&row, "seed")?;
        let increment: Option<i64> = opt(&row, "increment")?;
        raw.columns.push(RawColumn {
            object_id: get(&row, "object_id")?,
            name: get::<&str>(&row, "name")?.to_owned(),
            type_name: get::<&str>(&row, "type_name")?.to_owned(),
            max_length: get(&row, "max_length")?,
            precision: get(&row, "precision")?,
            scale: get(&row, "scale")?,
            is_nullable: get(&row, "is_nullable")?,
            is_computed: get(&row, "is_computed")?,
            is_user_defined_type: get(&row, "is_udt")?,
            identity: seed.zip(increment),
            default: opt::<&str>(&row, "default_definition")?.map(str::to_owned),
        });
    }

    for row in conn.query(KEY_COLUMNS).await? {
        raw.key_columns.push(RawKeyColumn {
            object_id: get(&row, "object_id")?,
            constraint_name: get::<&str>(&row, "name")?.to_owned(),
            is_primary: get(&row, "is_primary")?,
            column: get::<&str>(&row, "column_name")?.to_owned(),
        });
    }

    for row in conn.query(FOREIGN_KEY_COLUMNS).await? {
        raw.foreign_key_columns.push(RawForeignKeyColumn {
            object_id: get(&row, "object_id")?,
            constraint_name: get::<&str>(&row, "name")?.to_owned(),
            ref_schema: get::<&str>(&row, "ref_schema")?.to_owned(),
            ref_table: get::<&str>(&row, "ref_table")?.to_owned(),
            column: get::<&str>(&row, "column_name")?.to_owned(),
            ref_column: get::<&str>(&row, "ref_column_name")?.to_owned(),
            on_delete: get(&row, "delete_referential_action")?,
            on_update: get(&row, "update_referential_action")?,
        });
    }

    for row in conn.query(CHECKS).await? {
        raw.checks.push(RawCheck {
            object_id: get(&row, "object_id")?,
            name: get::<&str>(&row, "name")?.to_owned(),
            definition: get::<&str>(&row, "definition")?.to_owned(),
        });
    }

    for row in conn.query(INDEX_COLUMNS).await? {
        raw.index_columns.push(RawIndexColumn {
            object_id: get(&row, "object_id")?,
            index_name: get::<&str>(&row, "name")?.to_owned(),
            is_unique: get(&row, "is_unique")?,
            is_clustered: get(&row, "is_clustered")?,
            filter: opt::<&str>(&row, "filter_definition")?.map(str::to_owned),
            column: get::<&str>(&row, "column_name")?.to_owned(),
            is_included: get(&row, "is_included_column")?,
            is_descending: get(&row, "is_descending_key")?,
        });
    }

    for row in conn.query(MODULES).await? {
        let code = get::<&str>(&row, "type_code")?;
        // An unrecognised code means the query and the mapping have drifted;
        // skipping is right (it is not a module) but silence is not.
        let Some(kind) = crate::introspect::kind_from_type_code(code) else {
            continue;
        };
        let parent = opt::<&str>(&row, "parent_schema")?
            .zip(opt::<&str>(&row, "parent_table")?)
            .map(|(s, t)| (s.to_owned(), t.to_owned()));
        raw.modules.push(RawModule {
            schema: get::<&str>(&row, "schema_name")?.to_owned(),
            name: get::<&str>(&row, "object_name")?.to_owned(),
            kind,
            definition: opt::<&str>(&row, "definition")?.map(str::to_owned),
            parent,
        });
    }

    Ok(assemble(&raw))
}
