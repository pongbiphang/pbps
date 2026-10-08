//! The catalog queries behind [`crate::introspect`].
//!
//! Only this file runs SQL against a live server; it converts `sys.*` rows into
//! the plain `Raw*` structs and hands them to the pure assembler. The queries
//! are static — nothing user-controlled is ever interpolated into them.

use std::collections::BTreeMap;

use pbps_db::{Conn, DbError, FromColumn, Row};
use pbps_model::{ObservedRows, RowScope, Schema, TableName};

use crate::introspect::{
    IndexKind, Pulled, RawCatalog, RawCheck, RawColumn, RawForeignKeyColumn, RawIndexColumn,
    RawKeyColumn, RawModule, RawObjectDependency, RawTable, Securable, assemble,
};

/// `is_ms_shipped = 0` drops the system tables; the name list drops this tool's
/// own state and lock tables — they must never enter the managed set, or the
/// tool would plan changes to itself.
///
/// **By the qualified name, not by a prefix and not by the bare name.** Each
/// widening of that filter hides a table the project itself declared, which
/// nothing refuses: the pull reports it absent and the next plan tries to
/// create an object that is already there. `NOT LIKE '\_\_pbps\_%'` hid
/// `app.__pbps_customers`; `t.name NOT IN (…)` still hid `app.__pbps_state`.
/// What this tool owns is the two *qualified* names SPEC §8.1 defines — the
/// ledger lives in `dbo` and `pbps_db::ledger` spells it there — so the filter
/// asks for the schema too, and a step that adds a third table adds it here,
/// where a reader can see what the list is for.
///
/// **By the exact spelling, under a binary collation.** `is_ours` reserves only
/// the spelling pbps creates, so the filter must hide only that spelling too:
/// compared under a case-insensitive database collation, a project's own
/// `dbo.__PBPS_STATE` read as the ledger, and the pull reported a
/// declared table absent.
///
/// The PostgreSQL pull lists the same two names unqualified, because the schema
/// its ledger will live in is not decided until Phase 5 step 8 (#185).
/// Whether `name` is one of this tool's ledger tables, which the pull filters
/// out and validation therefore reserves (the PostgreSQL side's
/// `catalog::is_ours`). The exact spelling pbps creates, and no other:
/// validation runs offline and cannot know the database's collation, and on a
/// case-sensitive one `dbo.__PBPS_STATE` is a different table the
/// project may declare — the dialect's `fold_ident` preserves case for the
/// same reason.
pub(crate) fn is_ours(name: &pbps_model::TableName) -> bool {
    [crate::state::STATE_TABLE, crate::state::LOCK_TABLE]
        .contains(&format!("{}.{}", name.schema, name.name).as_str())
}

// The `|` appended to both sides is what makes the comparison exact: SQL Server
// pads `=` and `IN` operands with trailing spaces even under
// `Latin1_General_BIN2`, so a project's `dbo.[__pbps_state ]` compared equal to
// the ledger's name and was hidden from the pull (#954, measured on the pinned
// server). A trailing space now sits before the `|`, where padding cannot reach.
const TABLES: &str = "\
SELECT t.object_id, s.name AS schema_name, t.name AS table_name, t.temporal_type,
       CONVERT(bit, CASE WHEN p.object_id IS NULL THEN 0 ELSE 1 END) AS has_period,
       t.ledger_type, t.is_dropped_ledger_table, t.ledger_view_id,
       {temporal}
  FROM sys.tables t
  JOIN sys.schemas s ON s.schema_id = t.schema_id
  LEFT JOIN sys.periods p ON p.object_id = t.object_id
 WHERE t.is_ms_shipped = 0
   AND NOT ((s.name + N'|') COLLATE Latin1_General_BIN2 = N'dbo|'
            AND (t.name + N'|') COLLATE Latin1_General_BIN2
                IN (N'__pbps_state|', N'__pbps_lock|'))
 ORDER BY s.name, t.name;";

// SQL Server added the ledger columns of `sys.tables` in 2022. A server
// without them reads temporal metadata but must not be asked for them: naming
// a column the view lacks fails the whole pull. No ledger table can exist
// there.
const PRE_LEDGER_TABLES: &str = "\
SELECT t.object_id, s.name AS schema_name, t.name AS table_name, t.temporal_type,
       CONVERT(bit, CASE WHEN p.object_id IS NULL THEN 0 ELSE 1 END) AS has_period,
       CONVERT(tinyint, 0) AS ledger_type, CONVERT(bit, 0) AS is_dropped_ledger_table,
       CONVERT(int, NULL) AS ledger_view_id,
       {temporal}
  FROM sys.tables t
  JOIN sys.schemas s ON s.schema_id = t.schema_id
  LEFT JOIN sys.periods p ON p.object_id = t.object_id
 WHERE t.is_ms_shipped = 0
   AND NOT ((s.name + N'|') COLLATE Latin1_General_BIN2 = N'dbo|'
            AND (t.name + N'|') COLLATE Latin1_General_BIN2
                IN (N'__pbps_state|', N'__pbps_lock|'))
 ORDER BY s.name, t.name;";

// SQL Server added both `sys.tables.temporal_type` and `sys.periods` in 2016.
// Keep the whole legacy query free of those names: replacing only the selected
// column would still make an older server compile a join to a view it lacks.
const LEGACY_TABLES: &str = "\
SELECT t.object_id, s.name AS schema_name, t.name AS table_name,
       CONVERT(tinyint, 0) AS temporal_type, CONVERT(bit, 0) AS has_period,
       CONVERT(tinyint, 0) AS ledger_type, CONVERT(bit, 0) AS is_dropped_ledger_table,
       CONVERT(int, NULL) AS ledger_view_id,
       CONVERT(int, NULL) AS history_table_id,
       CONVERT(int, NULL) AS retention_period, CONVERT(int, NULL) AS retention_unit,
       CONVERT(sysname, NULL) AS period_start, CONVERT(sysname, NULL) AS period_end,
       CONVERT(tinyint, NULL) AS min_compression, CONVERT(tinyint, NULL) AS max_compression
  FROM sys.tables t
  JOIN sys.schemas s ON s.schema_id = t.schema_id
 WHERE t.is_ms_shipped = 0
   AND NOT ((s.name + N'|') COLLATE Latin1_General_BIN2 = N'dbo|'
            AND (t.name + N'|') COLLATE Latin1_General_BIN2
                IN (N'__pbps_state|', N'__pbps_lock|'))
 ORDER BY s.name, t.name;";

/// What a system-versioned table needs read beside it (#1176): its history,
/// retention, period columns, and its rows' compression, which a history
/// table is held to (DEC-1176.1). `{retention}` is the two retention columns,
/// which SQL Server added in 2017: a 2016 server is asked for neither, and
/// keeps every history row, which is what INFINITE (-1) says.
const TEMPORAL_COLUMNS: &str = "\
t.history_table_id,
       {retention},
       COL_NAME(p.object_id, p.start_column_id) AS period_start,
       COL_NAME(p.object_id, p.end_column_id) AS period_end,
       (SELECT MIN(pa.data_compression) FROM sys.partitions pa
         WHERE pa.object_id = t.object_id AND pa.index_id IN (0, 1)) AS min_compression,
       (SELECT MAX(pa.data_compression) FROM sys.partitions pa
         WHERE pa.object_id = t.object_id AND pa.index_id IN (0, 1)) AS max_compression";

/// `has_ledger` is the server's own answer to whether `sys.tables` carries
/// the ledger columns, not a guess from the banner: Azure SQL Edge says
/// "Azure" on a 15.x engine without them, and Azure SQL Database says 12.x on
/// one with them. `has_retention` is the same answer for the history
/// retention columns (2017).
fn tables_query(
    product_version: &str,
    edition: &str,
    has_ledger: bool,
    has_retention: bool,
) -> String {
    let retention = if has_retention {
        "t.history_retention_period AS retention_period, \
         t.history_retention_period_unit AS retention_unit"
    } else {
        "CONVERT(int, -1) AS retention_period, CONVERT(int, -1) AS retention_unit"
    };
    let temporal = TEMPORAL_COLUMNS.replace("{retention}", retention);
    if has_ledger {
        return TABLES.replace("{temporal}", &temporal);
    }
    if has_temporal(product_version, edition) {
        PRE_LEDGER_TABLES.replace("{temporal}", &temporal)
    } else {
        LEGACY_TABLES.to_owned()
    }
}

/// Whether the server has `sys.periods`, `sys.tables.temporal_type` and the
/// temporal columns of `sys.columns`: SQL Server 2016 (13.x) and later, and
/// every Azure SQL, whatever its banner says.
fn has_temporal(product_version: &str, edition: &str) -> bool {
    let major = product_version
        .split('.')
        .next()
        .and_then(|v| v.parse::<u32>().ok());
    edition.to_ascii_lowercase().contains("azure") || !major.is_some_and(|v| v < 13)
}

/// [`COLUMNS`], with the temporal columns of `sys.columns` where the server
/// has them (2016 and later); an older one has no period to read.
fn columns_query(temporal: bool) -> String {
    COLUMNS.replace(
        "{generated}",
        if temporal {
            "c.generated_always_type, c.is_hidden"
        } else {
            "CONVERT(tinyint, 0) AS generated_always_type, CONVERT(bit, 0) AS is_hidden"
        },
    )
}

const COLUMNS: &str = "\
SELECT c.object_id, c.name, ty.name AS type_name,
       c.max_length, c.precision, c.scale, c.is_nullable, c.is_computed,
       CONVERT(bit, CASE WHEN ty.is_user_defined = 1 THEN 1 ELSE 0 END) AS is_udt,
       CONVERT(bigint, ic.seed_value) AS seed,
       CONVERT(bigint, ic.increment_value) AS increment,
       dc.definition AS default_definition,
       dc.object_id AS default_constraint_object_id,
       dc.name AS default_constraint_name,
       -- NULL for every type but the character ones; compared in `assemble`
       -- against the database's own default collation (issue #94).
       c.collation_name,
       -- A computed column's expression and whether it is stored (#1174).
       -- The definition is NULL where the reader may not see it.
       cc.definition AS computed_definition,
       CONVERT(bit, ISNULL(cc.is_persisted, 0)) AS computed_persisted,
       -- A period's columns, and anything else the engine writes (#1176).
       {generated}
  FROM sys.columns c
  JOIN sys.types ty ON ty.user_type_id = c.user_type_id
  LEFT JOIN sys.computed_columns cc
         ON cc.object_id = c.object_id AND cc.column_id = c.column_id
  LEFT JOIN sys.identity_columns ic
         ON ic.object_id = c.object_id AND ic.column_id = c.column_id
  LEFT JOIN sys.default_constraints dc
         ON dc.parent_object_id = c.object_id AND dc.parent_column_id = c.column_id
 ORDER BY c.object_id, c.column_id;";

/// `rows_partitioned` says whether the table's rows (its heap or clustered
/// index) sit on a partition scheme, which no declaration can say (#1209
/// review).
///
/// `ki.type` travels for the same reason it does in `INDEX_COLUMNS`: the
/// declarations spell neither `CLUSTERED` nor `NONCLUSTERED` on a key, so a
/// key whose layout is not the engine's default for its kind would come back
/// from bootstrap with a different one (#1186).
const KEY_COLUMNS: &str = "\
SELECT kc.parent_object_id AS object_id, kc.name,
       CONVERT(bit, CASE WHEN kc.type = 'PK' THEN 1 ELSE 0 END) AS is_primary,
       col.name AS column_name, ki.is_disabled, ki.ignore_dup_key, ki.type AS index_type,
       CONVERT(bit, CASE WHEN EXISTS (
           SELECT 1 FROM sys.indexes b
             JOIN sys.data_spaces bd ON bd.data_space_id = b.data_space_id
            WHERE b.object_id = kc.parent_object_id AND b.index_id IN (0, 1) AND bd.type = 'PS')
         THEN 1 ELSE 0 END) AS rows_partitioned
  FROM sys.key_constraints kc
  JOIN sys.indexes ki
    ON ki.object_id = kc.parent_object_id AND ki.index_id = kc.unique_index_id
  JOIN sys.index_columns ic
    ON ic.object_id = kc.parent_object_id AND ic.index_id = kc.unique_index_id
  JOIN sys.columns col
    ON col.object_id = ic.object_id AND col.column_id = ic.column_id
 WHERE ic.is_included_column = 0
 ORDER BY kc.parent_object_id, kc.name, ic.key_ordinal;";

/// `ref_key_name` is the referenced table's key or unique index the engine
/// bound the foreign key to (`key_index_id`): a key the assembler leaves out
/// takes its foreign keys with it, or bootstrap would create a reference to a
/// candidate key that is not there.
const FOREIGN_KEY_COLUMNS: &str = "\
SELECT fk.parent_object_id AS object_id, fk.name,
       rs.name AS ref_schema, rt.name AS ref_table,
       fk.referenced_object_id AS ref_object_id, ri.name AS ref_key_name,
       pc.name AS column_name, rc.name AS ref_column_name,
       fk.delete_referential_action, fk.update_referential_action,
       fk.is_disabled, fk.is_not_trusted, fk.is_not_for_replication
  FROM sys.foreign_keys fk
  JOIN sys.foreign_key_columns fkc ON fkc.constraint_object_id = fk.object_id
  JOIN sys.columns pc
    ON pc.object_id = fkc.parent_object_id AND pc.column_id = fkc.parent_column_id
  JOIN sys.columns rc
    ON rc.object_id = fkc.referenced_object_id AND rc.column_id = fkc.referenced_column_id
  JOIN sys.tables rt ON rt.object_id = fk.referenced_object_id
  JOIN sys.schemas rs ON rs.schema_id = rt.schema_id
  LEFT JOIN sys.indexes ri
    ON ri.object_id = fk.referenced_object_id AND ri.index_id = fk.key_index_id
 ORDER BY fk.parent_object_id, fk.name, fkc.constraint_column_id;";

const CHECKS: &str = "\
SELECT cc.parent_object_id AS object_id, cc.object_id AS constraint_object_id,
       cc.name, cc.definition, cc.is_disabled, cc.is_not_trusted, cc.is_not_for_replication
  FROM sys.check_constraints cc
 WHERE cc.is_ms_shipped = 0
 ORDER BY cc.parent_object_id, cc.name;";

/// PK- and UNIQUE-backing indexes are already covered by `KEY_COLUMNS`;
/// hypothetical indexes are the tuning wizard's ghosts. Included columns sort
/// after key columns so the assembler sees keys in key order first.
///
/// `i.type` travels whole rather than collapsed into a clustered flag. Read as
/// `type = 1` and nothing else, every other physical kind — XML (3), spatial
/// (4), columnstore (5, 6), hash (7) — answered "not clustered" and was adopted
/// as the ordinary rowstore index it is not. The code is what lets the
/// assembler say which kind it left out.
const INDEX_COLUMNS: &str = "\
SELECT i.object_id, i.name, i.is_unique, i.type AS index_type, i.is_disabled, i.ignore_dup_key,
       i.filter_definition,
       col.name AS column_name, ic.is_included_column, ic.is_descending_key,
       CONVERT(bit, CASE WHEN EXISTS (
           SELECT 1 FROM sys.indexes b
             JOIN sys.data_spaces bd ON bd.data_space_id = b.data_space_id
            WHERE b.object_id = i.object_id AND b.index_id IN (0, 1) AND bd.type = 'PS')
         THEN 1 ELSE 0 END) AS rows_partitioned
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
SELECT o.object_id, NULLIF(o.parent_object_id, 0) AS parent_object_id,
       s.name AS schema_name, o.name AS object_name, o.type AS type_code,
       m.definition AS definition,
       ps.name AS parent_schema, pt.name AS parent_table,
       -- Persisted with the module and re-applied on every execution, so they
       -- are part of what it does. NULL for a module with no readable
       -- definition, which is refused for its own reason first.
       CONVERT(bit, ISNULL(m.uses_quoted_identifier, 1)) AS quoted_identifier,
       CONVERT(bit, ISNULL(m.uses_ansi_nulls, 1)) AS ansi_nulls,
       CONVERT(bit, ISNULL(m.is_schema_bound, 0)) AS schema_bound
  FROM sys.objects o
  JOIN sys.schemas s ON s.schema_id = o.schema_id
  LEFT JOIN sys.sql_modules m ON m.object_id = o.object_id
  -- sys.objects rather than sys.tables for the parent: a trigger may be
  -- attached to a view (INSTEAD OF), and joining only tables would leave it
  -- with no `on:` and a declaration that cannot be loaded back.
  LEFT JOIN sys.objects pt ON pt.object_id = o.parent_object_id
  LEFT JOIN sys.schemas ps ON ps.schema_id = pt.schema_id
 WHERE o.is_ms_shipped = 0
   AND o.type IN ('V', 'P', 'PC', 'FN', 'IF', 'TF', 'FS', 'FT', 'TR')
 ORDER BY s.name, o.name;";

// Only dependencies the engine resolved to an object in this database can
// name a temporal table read above. Unresolved and cross-database references
// have no `referenced_id` and cannot be matched safely by text.
//
// Asked object by object, of the modules and the default and check
// constraints the assembler follows, and not of `sys.sql_expression_dependencies`: that view
// returns no row at all without database `VIEW DEFINITION`, which SPEC §9.5
// does not ask for, and the omission closure then read as nothing depending
// on anything (#1644, DEC-1644.1). The function answers under the
// managed-schema grant, and `public` holds `SELECT` on it in `master`.
// A CLR module's dependencies are not in its text, and neither source has
// any for it, so it is not asked.
//
// Inside `TRY`, because the function raises Msg 207 and 2020 for a referrer
// that no longer binds (a column renamed or dropped under a module that is not
// schema-bound), and the client turns those into a failed read: one stale
// procedure anywhere would stop every pull. Inside `TRY` the same rows come
// back with no error sent, the stale referrer's edge included, and as `sa` they
// match the catalog view's exactly (measured on SQL Server 2017, 2019, 2022
// and 2025). The `CATCH` raises the error again: whatever does transfer
// control is a read that failed, never one that found nothing. With
// `RAISERROR` rather than `THROW`, which SQL Server 2008 cannot parse, and the
// pull still reads a server that old.
//
// A reference into another database keeps that database's `referenced_id`,
// where the view has `NULL`, and object ids repeat across databases: two fresh
// databases measured with the same id for their first table on 17.0. Such a row
// would read as an edge to whatever local object holds the number, so only
// rows naming no database, or this one by a three-part name, are edges.
const MODULE_DEPENDENCIES: &str = "\
BEGIN TRY
SELECT DISTINCT o.object_id AS referencing_id, r.referenced_id
  FROM sys.objects o
  JOIN sys.schemas s ON s.schema_id = o.schema_id
 CROSS APPLY sys.dm_sql_referenced_entities(
       QUOTENAME(s.name) + N'.' + QUOTENAME(o.name), N'OBJECT') r
 WHERE o.is_ms_shipped = 0
   AND o.type IN ('V', 'P', 'FN', 'IF', 'TF', 'TR', 'D', 'C')
   AND r.referenced_id IS NOT NULL
   AND (r.referenced_database_name IS NULL OR DB_ID(r.referenced_database_name) = DB_ID())
 ORDER BY referencing_id, r.referenced_id;
END TRY
BEGIN CATCH
    DECLARE @error nvarchar(2048) = ERROR_MESSAGE();
    RAISERROR(N'%s', 16, 1, @error);
END CATCH;";

// Non-schema-bound FN and TF functions can be created while a referenced
// table is absent (pinned against SQL Server by the deferred-resolution live
// test). Including them in the temporal omission closure would discard
// modules bootstrap can create; views, inline functions and schema-bound
// functions require their references to exist already.
fn requires_bound_references(type_code: &str, schema_bound: bool) -> bool {
    matches!(type_code.trim(), "V" | "IF") || schema_bound
}

/// User-defined database roles (ADR-0005). `is_fixed_role = 0` drops
/// `db_owner` and friends; `public` is type `R` and not fixed, so it is
/// excluded by name.
const ROLES: &str = "\
SELECT p.name
  FROM sys.database_principals p
 WHERE p.type = 'R' AND p.is_fixed_role = 0 AND p.name <> 'public'
 ORDER BY p.name;";

/// Every permission held by a user-defined role, of every class. Column-level
/// rows come too (`minor_id <> 0`), and so do the database-level ones
/// (class 0: `CONTROL`, `CREATE TABLE`) and every other class the model does
/// not hold, so the assembler can report each rather than have it silently
/// absent — a `GRANT CONTROL TO role` that the read never saw compared equal
/// on the grants it did see (DECISIONS 105).
///
/// The securable is resolved through `sys.all_objects`, not `sys.objects`: a
/// grant on a system object — `GRANT SELECT ON sys.objects`, `GRANT EXECUTE ON
/// sys.sp_executesql` — has a `major_id` that only `sys.all_objects` holds, and
/// against `sys.objects` alone it came back nameless. A nameless securable is
/// reported rather than dropped, so such a grant, on an object no project
/// manages, would refuse every connected command; named, the managed-set filter
/// discards it the way it discards any other grant on somebody else's object.
/// What stays nameless is then only what this connection may not see.
const PERMISSIONS: &str = "\
SELECT pr.name AS role_name, dp.class, dp.class_desc, dp.permission_name, dp.state, dp.minor_id,
       COALESCE(os.name, ss.name) AS schema_name, o.name AS object_name
  FROM sys.database_permissions dp
  JOIN sys.database_principals pr ON pr.principal_id = dp.grantee_principal_id
  LEFT JOIN sys.all_objects o ON dp.class = 1 AND o.object_id = dp.major_id
  LEFT JOIN sys.schemas os ON os.schema_id = o.schema_id
  LEFT JOIN sys.schemas ss ON dp.class = 3 AND ss.schema_id = dp.major_id
 WHERE pr.type = 'R' AND pr.is_fixed_role = 0 AND pr.name <> 'public'
 ORDER BY pr.name, dp.class, schema_name, object_name, dp.permission_name;";

/// A required value that came back NULL means the query and the struct have
/// drifted apart; that is a bug here, not bad data, and it must be named.
pub(crate) fn get<'a, T: FromColumn<'a>>(row: &'a Row, col: &str) -> Result<T, DbError> {
    row.try_get::<T>(col)?
        .ok_or_else(|| DbError::BadRow(format!("column `{col}` is unexpectedly NULL")))
}

pub(crate) fn opt<'a, T: FromColumn<'a>>(row: &'a Row, col: &str) -> Result<Option<T>, DbError> {
    row.try_get::<T>(col)
}

/// Reads the whole catalog and assembles it into a schema.
pub async fn introspect(conn: &mut Conn) -> Result<Pulled, DbError> {
    let mut raw = RawCatalog::default();

    // Temporal metadata arrived in 2016; merely referencing the column fails
    // on older servers. Azure's 12.x banner is not SQL Server 2014, and an
    // unreadable version must not silently classify temporal tables as plain.
    // The retention probe is shared with the connected check that refuses
    // what a server without it cannot take (#1502).
    let versions = conn
        .query(&format!(
            "SELECT CONVERT(nvarchar(128), SERVERPROPERTY('ProductVersion')) AS version,
                CONVERT(nvarchar(128), SERVERPROPERTY('Edition')) AS edition,
                CONVERT(nvarchar(128), DATABASEPROPERTYEX(DB_NAME(), 'Collation')) AS db_collation,
                CONVERT(bit, CASE WHEN COL_LENGTH('sys.tables', 'ledger_type') IS NULL
                                  THEN 0 ELSE 1 END) AS has_ledger,
                CONVERT(bit, {}) AS has_retention;",
            crate::temporal::RETENTION_PROBE
        ))
        .await?;
    let version = versions
        .first()
        .ok_or_else(|| DbError::BadRow("the server version query returned no row".into()))?;
    let tables = tables_query(
        get(version, "version")?,
        get(version, "edition")?,
        get(version, "has_ledger")?,
        get(version, "has_retention")?,
    );
    let columns = columns_query(has_temporal(
        get(version, "version")?,
        get(version, "edition")?,
    ));
    raw.database_collation = get::<&str>(version, "db_collation")?.to_owned();
    for row in conn.query(&tables).await? {
        raw.tables.push(RawTable {
            object_id: get(&row, "object_id")?,
            schema: get::<&str>(&row, "schema_name")?.to_owned(),
            name: get::<&str>(&row, "table_name")?.to_owned(),
            temporal_type: get(&row, "temporal_type")?,
            has_period: get(&row, "has_period")?,
            ledger_type: get(&row, "ledger_type")?,
            is_dropped_ledger_table: get(&row, "is_dropped_ledger_table")?,
            ledger_view_id: opt(&row, "ledger_view_id")?,
            history_table_id: opt(&row, "history_table_id")?,
            retention: opt::<i32>(&row, "retention_period")?
                .zip(opt::<i32>(&row, "retention_unit")?),
            period: opt::<&str>(&row, "period_start")?
                .map(str::to_owned)
                .zip(opt::<&str>(&row, "period_end")?.map(str::to_owned)),
            compression: opt::<u8>(&row, "min_compression")?
                .zip(opt::<u8>(&row, "max_compression")?),
        });
    }

    for row in conn.query(&columns).await? {
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
            default_constraint: opt::<i32>(&row, "default_constraint_object_id")?
                .zip(opt::<&str>(&row, "default_constraint_name")?.map(str::to_owned)),
            collation: opt::<&str>(&row, "collation_name")?.map(str::to_owned),
            computed_definition: opt::<&str>(&row, "computed_definition")?.map(str::to_owned),
            computed_persisted: get(&row, "computed_persisted")?,
            generated_always_type: get(&row, "generated_always_type")?,
            is_hidden: get(&row, "is_hidden")?,
        });
    }

    for row in conn.query(KEY_COLUMNS).await? {
        raw.key_columns.push(RawKeyColumn {
            is_disabled: get(&row, "is_disabled")?,
            ignore_dup_key: get(&row, "ignore_dup_key")?,
            index_type: get(&row, "index_type")?,
            rows_partitioned: get(&row, "rows_partitioned")?,

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
            ref_object_id: get(&row, "ref_object_id")?,
            ref_key: opt::<&str>(&row, "ref_key_name")?.map(str::to_owned),
            column: get::<&str>(&row, "column_name")?.to_owned(),
            ref_column: get::<&str>(&row, "ref_column_name")?.to_owned(),
            on_delete: get(&row, "delete_referential_action")?,
            on_update: get(&row, "update_referential_action")?,
            is_disabled: get(&row, "is_disabled")?,
            is_not_trusted: get(&row, "is_not_trusted")?,
            is_not_for_replication: get(&row, "is_not_for_replication")?,
        });
    }

    for row in conn.query(CHECKS).await? {
        raw.checks.push(RawCheck {
            is_disabled: get(&row, "is_disabled")?,
            is_not_trusted: get(&row, "is_not_trusted")?,
            is_not_for_replication: get(&row, "is_not_for_replication")?,

            object_id: get(&row, "object_id")?,
            constraint_object_id: get(&row, "constraint_object_id")?,
            name: get::<&str>(&row, "name")?.to_owned(),
            definition: get::<&str>(&row, "definition")?.to_owned(),
        });
    }

    for row in conn.query(INDEX_COLUMNS).await? {
        raw.index_columns.push(RawIndexColumn {
            is_disabled: get(&row, "is_disabled")?,
            ignore_dup_key: get(&row, "ignore_dup_key")?,

            object_id: get(&row, "object_id")?,
            index_name: get::<&str>(&row, "name")?.to_owned(),
            is_unique: get(&row, "is_unique")?,
            kind: IndexKind::from_type_code(get::<u8>(&row, "index_type")?),
            filter: opt::<&str>(&row, "filter_definition")?.map(str::to_owned),
            column: get::<&str>(&row, "column_name")?.to_owned(),
            is_included: get(&row, "is_included_column")?,
            is_descending: get(&row, "is_descending_key")?,
            rows_partitioned: get(&row, "rows_partitioned")?,
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
        let quoted: bool = get(&row, "quoted_identifier")?;
        let ansi_nulls: bool = get(&row, "ansi_nulls")?;
        let schema_bound: bool = get(&row, "schema_bound")?;
        raw.modules.push(RawModule {
            object_id: get(&row, "object_id")?,
            parent_object_id: opt(&row, "parent_object_id")?,
            default_set_options: quoted && ansi_nulls,
            schema: get::<&str>(&row, "schema_name")?.to_owned(),
            name: get::<&str>(&row, "object_name")?.to_owned(),
            kind,
            definition: opt::<&str>(&row, "definition")?.map(str::to_owned),
            parent,
            requires_bound_references: requires_bound_references(code, schema_bound),
        });
    }

    for row in conn.query(MODULE_DEPENDENCIES).await? {
        raw.object_dependencies.push(RawObjectDependency {
            referencing_object_id: get(&row, "referencing_id")?,
            referenced_object_id: get(&row, "referenced_id")?,
        });
    }

    for row in conn.query(ROLES).await? {
        raw.roles.push(crate::introspect::RawRole {
            name: get::<&str>(&row, "name")?.to_owned(),
        });
    }

    for row in conn.query(PERMISSIONS).await? {
        let class: u8 = get(&row, "class")?;
        // The joined name is NULL for two reasons that the row cannot tell
        // apart, and neither is "there is nothing here": an object this
        // connection may not see yields no name while its permission row
        // still arrives, and so would a dropped object's orphaned row. The
        // grant is being read either way and its securable cannot be named,
        // so it travels as unreadable and the assembler reports it. Dropped
        // here, `pull` wrote a role narrower than the database holds.
        let securable = match (class, opt::<&str>(&row, "schema_name")?) {
            (1, Some(schema)) => match opt::<&str>(&row, "object_name")? {
                Some(name) => Securable::Object {
                    schema: schema.to_owned(),
                    name: name.to_owned(),
                },
                None => Securable::Unreadable,
            },
            (3, Some(schema)) => Securable::Schema(schema.to_owned()),
            (1 | 3, None) => Securable::Unreadable,
            // Every other class names nothing a declaration could hold, and
            // has no schema to begin with.
            _ => Securable::Unnamed,
        };
        raw.permissions.push(crate::introspect::RawPermission {
            role: get::<&str>(&row, "role_name")?.to_owned(),
            class,
            class_desc: get::<&str>(&row, "class_desc")?.trim().to_owned(),
            permission: get::<&str>(&row, "permission_name")?.trim().to_owned(),
            state: get::<&str>(&row, "state")?.to_owned(),
            securable,
            minor_id: get(&row, "minor_id")?,
        });
    }

    Ok(assemble(&raw))
}

/// Who holds each user-defined role, by role name (ADR-0005).
///
/// Read only by `plan --db`, and only to list the members a `DROP ROLE` has
/// to remove first: membership is each environment's own and is never
/// compared, but a role cannot be dropped while it has members, and a plan
/// that removes them has to say whom. A nested role is a member like any
/// user and comes back the same way.
/// The database principals holding any of `names`, as the engine compares
/// names: one `(declared, held, kind)` per collision, `held` being the
/// principal's own spelling and `kind` the catalog's word for what it is,
/// in lower case with spaces (`sql user`, `database role`, `application
/// role`). Users, roles and application roles share one namespace, and a
/// `CREATE ROLE` or `ALTER ROLE ... WITH NAME` onto a taken name fails
/// after everything ordered before it has run — so a declared role's name
/// is checked against these before a connected plan is written and again
/// before it is applied (DECISIONS 118, 119).
///
/// Asked of the engine under the catalog collation (see
/// [`object_names_alike`], #1243) rather than compared
/// here: `Shadow` and `shadow` are one name to a case-insensitive database
/// and two to a `BTreeMap`, and a map lookup said the name was free.
/// `except` names the principals this plan vacates — the roles it drops or
/// renames away — which hold their name only until the statement that
/// frees it, and are excluded the same way, by the engine.
pub async fn principals_holding(
    conn: &mut Conn,
    names: &[&str],
    except: &[&str],
) -> Result<Vec<(String, String, String)>, DbError> {
    if names.is_empty() {
        return Ok(Vec::new());
    }
    let values = |names: &[&str]| {
        names
            .iter()
            .map(|n| format!("({})", crate::ident::literal(n)))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let mut sql = format!(
        "SELECT d.name AS declared, p.name AS held, p.type_desc
           FROM (VALUES {}) AS d(name)
           JOIN sys.database_principals AS p
             ON p.name = d.name COLLATE CATALOG_DEFAULT",
        values(names)
    );
    if !except.is_empty() {
        sql.push_str(&format!(
            "
 WHERE NOT EXISTS (SELECT 1 FROM (VALUES {}) AS v(name)
                                WHERE v.name = p.name COLLATE CATALOG_DEFAULT)",
            values(except)
        ));
    }
    sql.push_str(
        "
 ORDER BY d.name, p.name;",
    );
    let mut out = Vec::new();
    for row in conn.query(&sql).await? {
        out.push((
            get::<&str>(&row, "declared")?.to_owned(),
            get::<&str>(&row, "held")?.to_owned(),
            get::<&str>(&row, "type_desc")?
                .trim()
                .to_ascii_lowercase()
                .replace('_', " "),
        ));
    }
    Ok(out)
}

/// Compare captured table names under the catalog collation that names them,
/// part by part (DECISIONS 119, 142). Only values from the caller's read are
/// compared: a second catalog lookup could miss a name that read already saw.
/// The connected database's default collation (#1175), as the pull reads it.
pub async fn database_collation(conn: &mut Conn) -> Result<String, DbError> {
    let rows = conn
        .query("SELECT CONVERT(nvarchar(128), DATABASEPROPERTYEX(DB_NAME(), 'Collation')) AS c;")
        .await?;
    let Some(name) = rows.first().map(|r| get::<&str>(r, "c")).transpose()? else {
        return Err(DbError::Refused(
            "the database's default collation could not be read".to_owned(),
        ));
    };
    Ok(name.to_owned())
}

/// The declared collation names this server does not have (#1175).
///
/// Asked of `sys.fn_helpcollations()`, which lists every collation the
/// engine accepts; a name it lacks is refused by the `CREATE` or `ALTER` that
/// names it (448, "Invalid collation"), after every statement ahead of it has
/// run. The comparison is case-insensitive under a binary collation of the
/// uppercased names, the way the engine resolves a collation name — measured,
/// `latin1_general_ci_as` names `Latin1_General_CI_AS`.
///
/// The names are collated *before* `UPPER`, which case-folds under its
/// input's collation (#1247 review). Measured on 17.0, on a
/// `Turkish_100_CI_AS` database, `UPPER(N'i')` is `İ` (U+0130): uppercased
/// under the database default and collated afterwards, a lowercase name the
/// engine accepts matched nothing and was refused as unknown.
pub async fn unknown_collations(conn: &mut Conn, names: &[String]) -> Result<Vec<String>, DbError> {
    if names.is_empty() {
        return Ok(Vec::new());
    }
    let values = names
        .iter()
        .enumerate()
        .map(|(i, n)| format!("({i}, {})", crate::ident::literal(n)))
        .collect::<Vec<_>>()
        .join(", ");
    let sql = format!(
        "SELECT w.i FROM (VALUES {values}) AS w(i, name)
          WHERE NOT EXISTS (
                SELECT 1 FROM sys.fn_helpcollations() h
                 WHERE UPPER(h.name COLLATE Latin1_General_BIN2)
                     = UPPER(w.name COLLATE Latin1_General_BIN2))
          ORDER BY w.i;"
    );
    let mut unknown = Vec::new();
    for row in conn.query(&sql).await? {
        let i: i32 = get(&row, "i")?;
        if let Some(name) = usize::try_from(i).ok().and_then(|i| names.get(i)) {
            unknown.push(name.clone());
        }
    }
    Ok(unknown)
}

pub async fn matching_table_names(
    conn: &mut Conn,
    wanted: &[TableName],
    observed: &[TableName],
) -> Result<Vec<TableName>, DbError> {
    if wanted.is_empty() || observed.is_empty() {
        return Ok(Vec::new());
    }
    let values = |names: &[TableName]| {
        names
            .iter()
            .enumerate()
            .map(|(i, n)| {
                format!(
                    "({i}, {}, {})",
                    crate::ident::literal(&n.schema),
                    crate::ident::literal(&n.name)
                )
            })
            .collect::<Vec<_>>()
            .join(", ")
    };
    let sql = format!(
        "SELECT DISTINCT a.i FROM (VALUES {}) AS a(i, schema_name, table_name)
         JOIN (VALUES {}) AS b(i, schema_name, table_name)
           ON a.schema_name = b.schema_name COLLATE CATALOG_DEFAULT
          AND a.table_name = b.table_name COLLATE CATALOG_DEFAULT ORDER BY a.i;",
        values(wanted),
        values(observed)
    );
    let mut found = Vec::new();
    for row in conn.query(&sql).await? {
        let i = get::<i32>(&row, "i")? as usize;
        found.push(wanted[i].clone());
    }
    Ok(found)
}

/// One edge of `sys.sql_expression_dependencies` (#1431): what an object's
/// stored text names, as the engine resolved it when it stored the text.
///
/// The engine's own answer, by object id, where a text scan answers by
/// spelling: it tells `dbo.f` from `x.f`, reads `[cafe]` as the column it
/// binds under the collation, and knows which modules are schema-bound
/// (measured on 17.0, comment on #1431).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExpressionEdge {
    /// The referencing object: a table for a computed column's edge, or a
    /// module.
    pub from: TableName,
    /// The computed column whose expression this is, for a table's edge.
    pub from_column: Option<String>,
    /// Whether the referencing module is `WITH SCHEMABINDING`
    /// (`sys.sql_modules.is_schema_bound`). False for a table's edge.
    pub from_schema_bound: bool,
    /// The referenced object.
    pub to: TableName,
    /// The referenced column, where the edge names one.
    pub to_column: Option<String>,
    /// The referenced object's `sys.objects.type`, trimmed: `U` for a table,
    /// `FN` for a scalar function, `V` for a view.
    pub to_kind: String,
}

/// Every edge whose referencing or referenced object is one of `objects`
/// (#1431). An edge whose referenced object the engine did not resolve has
/// no id to join, and is not one: a computed column cannot name an object
/// the engine has not resolved.
///
/// Only a computed column's edge or a module's: a filtered index (class 7)
/// and filtered statistics (9) record edges too, with an index or stats id
/// in `referencing_minor_id` that `COL_NAME` would read as some column, and
/// a check constraint is an object of its own (measured on 17.0).
pub async fn expression_edges(
    conn: &mut Conn,
    objects: &[TableName],
) -> Result<Vec<ExpressionEdge>, DbError> {
    if objects.is_empty() {
        return Ok(Vec::new());
    }
    require_dependency_catalog(conn).await?;
    let values = objects
        .iter()
        .map(|n| {
            format!(
                "({}, {})",
                crate::ident::literal(&n.schema),
                crate::ident::literal(&n.name)
            )
        })
        .collect::<Vec<_>>()
        .join(", ");
    let sql = format!(
        "SELECT rs.name AS from_schema, ro.name AS from_name,
                cc.name AS from_column,
                CONVERT(bit, ISNULL(m.is_schema_bound, 0)) AS from_schema_bound,
                es.name AS to_schema, eo.name AS to_name,
                CASE WHEN d.referenced_minor_id > 0
                     THEN COL_NAME(d.referenced_id, d.referenced_minor_id) END AS to_column,
                RTRIM(CONVERT(nvarchar(2), eo.type)) AS to_kind
           FROM sys.sql_expression_dependencies d
           JOIN sys.objects ro ON ro.object_id = d.referencing_id
           JOIN sys.schemas rs ON rs.schema_id = ro.schema_id
           JOIN sys.objects eo ON eo.object_id = d.referenced_id
           JOIN sys.schemas es ON es.schema_id = eo.schema_id
           LEFT JOIN sys.sql_modules m ON m.object_id = d.referencing_id
           LEFT JOIN sys.computed_columns cc
             ON cc.object_id = d.referencing_id AND cc.column_id = d.referencing_minor_id
          WHERE d.referencing_class = 1 AND d.referenced_class = 1
            AND (cc.column_id IS NOT NULL OR (m.object_id IS NOT NULL AND d.referencing_minor_id = 0))
            AND EXISTS (SELECT 1 FROM (VALUES {values}) AS w(schema_name, object_name)
                         WHERE (w.schema_name = rs.name COLLATE CATALOG_DEFAULT
                                AND w.object_name = ro.name COLLATE CATALOG_DEFAULT)
                            OR (w.schema_name = es.name COLLATE CATALOG_DEFAULT
                                AND w.object_name = eo.name COLLATE CATALOG_DEFAULT))
          ORDER BY rs.name, ro.name, from_column, es.name, eo.name, to_column;"
    );
    let mut out = Vec::new();
    for row in conn.query(&sql).await? {
        out.push(ExpressionEdge {
            from: TableName::new(
                get::<&str>(&row, "from_schema")?,
                get::<&str>(&row, "from_name")?,
            ),
            from_column: opt::<&str>(&row, "from_column")?.map(str::to_owned),
            from_schema_bound: get(&row, "from_schema_bound")?,
            to: TableName::new(
                get::<&str>(&row, "to_schema")?,
                get::<&str>(&row, "to_name")?,
            ),
            to_column: opt::<&str>(&row, "to_column")?.map(str::to_owned),
            to_kind: get::<&str>(&row, "to_kind")?.to_owned(),
        });
    }
    Ok(out)
}

/// Whether any of `tables` may hold a computed column that reads a column
/// the plan changes (#1462). Only such a table's own computed columns can
/// refuse a column's rename, drop or retype, and `sys.computed_columns` shows
/// them to a login holding `VIEW DEFINITION` on the table's schema, which
/// `sys.sql_expression_dependencies` does not. A table this login cannot see
/// answers yes: absent is not "no computed column".
pub async fn may_hold_computed_columns(
    conn: &mut Conn,
    tables: &[TableName],
) -> Result<bool, DbError> {
    if tables.is_empty() {
        return Ok(false);
    }
    let values = tables
        .iter()
        .map(|n| {
            format!(
                "({}, {})",
                crate::ident::literal(&n.schema),
                crate::ident::literal(&n.name)
            )
        })
        .collect::<Vec<_>>()
        .join(", ");
    let sql = format!(
        "SELECT COUNT(*) AS n
           FROM (VALUES {values}) AS w(schema_name, table_name)
          WHERE NOT EXISTS (
                  SELECT 1 FROM sys.tables t
                    JOIN sys.schemas s ON s.schema_id = t.schema_id
                   WHERE s.name = w.schema_name COLLATE CATALOG_DEFAULT
                     AND t.name = w.table_name COLLATE CATALOG_DEFAULT)
             OR EXISTS (
                  SELECT 1 FROM sys.computed_columns c
                    JOIN sys.tables t ON t.object_id = c.object_id
                    JOIN sys.schemas s ON s.schema_id = t.schema_id
                   WHERE s.name = w.schema_name COLLATE CATALOG_DEFAULT
                     AND t.name = w.table_name COLLATE CATALOG_DEFAULT);"
    );
    let rows = conn.query(&sql).await?;
    let row = rows
        .first()
        .ok_or_else(|| DbError::Refused("the computed-column count returned no row".into()))?;
    Ok(get::<i32>(row, "n")? > 0)
}

/// What a hidden referrer can keep the connected pass of DEC-1431.1 from
/// deciding (#1462, #1643 review): a function the plan alters or drops, which
/// a computed column anywhere may call, and a computed column the plan drops,
/// which a schema-bound module anywhere may read. A column change is judged
/// by the computed columns of its own table, which are visible with it, so a
/// hidden referrer elsewhere decides nothing there.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ReferrerTargets {
    pub functions: Vec<TableName>,
    pub computed_columns: Vec<(TableName, String)>,
}

/// Proves the connected pass may read [`expression_edges`] as complete
/// before it does (#1462). An empty read is otherwise "no edge" whether there
/// is none or this login cannot see it, and the pass would neither refuse an
/// alter or drop of a function a hidden computed column calls nor move it
/// (3729 at apply).
///
/// Measured on 17.0 (DEC-1462.1): `sys.sql_expression_dependencies` returns
/// no row at all without database `VIEW DEFINITION`, so that grant is asked
/// whenever the edges are read. With it, the edge from an object hidden by a
/// schema or object `DENY` stays, and only its referencing object is missing
/// from `sys.objects`: such an edge onto one of `targets` refuses the plan,
/// naming the target. A `db_owner` member's override shows the object, and
/// passes.
pub async fn prove_referrers_visible(
    conn: &mut Conn,
    targets: &ReferrerTargets,
) -> Result<(), DbError> {
    require_dependency_catalog(conn).await?;
    let Some(sql) = hidden_referrers_query(targets) else {
        return Ok(());
    };
    let hidden: Vec<String> = conn
        .query(&sql)
        .await?
        .iter()
        .map(|row| {
            let object =
                TableName::new(get::<&str>(row, "to_schema")?, get::<&str>(row, "to_name")?);
            Ok(match opt::<&str>(row, "to_column")? {
                Some(column) => format!("`{object}.{column}`"),
                None => format!("`{object}`"),
            })
        })
        .collect::<Result<_, DbError>>()?;
    if !hidden.is_empty() {
        return Err(DbError::Refused(format!(
            "a referrer of {} is hidden from this login: a DENY of VIEW DEFINITION or CONTROL \
             on its schema or object keeps the catalog from saying whether it calls what this \
             plan changes",
            hidden.join(", ")
        )));
    }
    Ok(())
}

/// The one guard every read of `sys.sql_expression_dependencies` passes
/// (#1644, DEC-1644.1). The view returns no row at all, and no error, without
/// database `VIEW DEFINITION` (measured on SQL Server 2017 to 2025), so an
/// empty read is "no
/// edge" only behind this proof. Only the hidden-referrer proof and the edges
/// it covers read the view: it alone keeps the edge of a referrer this login
/// cannot see, with a `NULL` referrer, where `sys.dm_sql_referenc*_entities`
/// drop it. So does a rename's impact on SQL Server 2008 to 2012, where those
/// functions want `CONTROL` on the table ([`crate::impact::DependencyRead`]).
/// Every other dependency read asks those functions, which answer under the
/// managed-schema grant.
pub(crate) async fn require_dependency_catalog(conn: &mut Conn) -> Result<(), DbError> {
    let granted = conn
        .query("SELECT HAS_PERMS_BY_NAME(DB_NAME(), 'DATABASE', 'VIEW DEFINITION') AS granted;")
        .await?;
    if granted
        .first()
        .map(|row| opt::<i32>(row, "granted"))
        .transpose()?
        .flatten()
        != Some(1)
    {
        return Err(DbError::Refused(
            "this login does not hold database VIEW DEFINITION, so \
             sys.sql_expression_dependencies returns no edge at all and a computed column or \
             module that calls what this plan changes cannot be seen"
                .into(),
        ));
    }
    Ok(())
}

/// The targets an edge this login cannot attribute refers to: the engine's
/// row is there, and `sys.objects` does not show its referencing object. A
/// function is matched whole; a computed column only by an edge naming it.
/// Only a schema-bound reference can block either change: a computed
/// column's call and a `WITH SCHEMABINDING` module's read are, and the row
/// says so when its referrer is hidden; a plain procedure or view calling the
/// function, or reading the column, lets both run (measured on 17.0).
fn hidden_referrers_query(targets: &ReferrerTargets) -> Option<String> {
    let lit = crate::ident::literal;
    let values = targets
        .functions
        .iter()
        .map(|n| {
            format!(
                "({}, {}, CONVERT(sysname, NULL))",
                lit(&n.schema),
                lit(&n.name)
            )
        })
        .chain(targets.computed_columns.iter().map(|(n, c)| {
            format!(
                "({}, {}, CONVERT(sysname, {}))",
                lit(&n.schema),
                lit(&n.name),
                lit(c)
            )
        }))
        .collect::<Vec<_>>();
    if values.is_empty() {
        return None;
    }
    Some(format!(
        "SELECT DISTINCT es.name AS to_schema, eo.name AS to_name, w.column_name AS to_column
           FROM sys.sql_expression_dependencies d
           JOIN sys.objects eo ON eo.object_id = d.referenced_id
           JOIN sys.schemas es ON es.schema_id = eo.schema_id
           JOIN (VALUES {}) AS w(schema_name, object_name, column_name)
             ON w.schema_name = es.name COLLATE CATALOG_DEFAULT
            AND w.object_name = eo.name COLLATE CATALOG_DEFAULT
            AND (w.column_name IS NULL
                 OR w.column_name = COL_NAME(d.referenced_id, d.referenced_minor_id)
                                    COLLATE CATALOG_DEFAULT)
          WHERE d.referencing_class = 1 AND d.referenced_class = 1
            AND d.is_schema_bound_reference = 1
            AND NOT EXISTS (SELECT 1 FROM sys.objects ro WHERE ro.object_id = d.referencing_id)
          ORDER BY to_schema, to_name, to_column;",
        values.join(", ")
    ))
}

/// An object at a name a plan creates (#1077). SQL Server keeps tables,
/// views, routines, triggers, sequences, synonyms and constraints in one
/// `sys.objects` namespace per schema, so `CREATE TABLE` at any of their
/// names fails with Msg 2714, and `CREATE OR ALTER` at a module's name
/// fails or replaces it. The inventory reads tables and modules, never
/// sequences, synonyms or constraints, so the names are asked here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NameOccupant {
    /// The name as the plan spells it.
    pub wanted: TableName,
    /// The name as the catalog spells it, which the database's collation may
    /// match to `wanted` without being equal to it.
    pub name: TableName,
    /// Its `type_desc`, lower-cased and spaced: `sequence object`, `synonym`,
    /// `check constraint`.
    pub kind: String,
    /// The table or view a constraint or trigger belongs to.
    pub parent: Option<TableName>,
    /// The column a default constraint belongs to, which goes with the
    /// column and with the default it is.
    pub parent_column: Option<String>,
}

/// Whether everything in `schemas` is visible to this connection, which an
/// absent row from [`object_name_occupants`] has to mean before it can be
/// read as a free name (#1192). `sys.objects` is filtered by metadata
/// visibility: an object under an effective `DENY VIEW DEFINITION` returns no
/// row, and a hidden sequence or synonym would read as free.
///
/// DEC-1192.1: the proof `impact::key_drop_blockers` uses (DECISIONS 460),
/// measured on the pinned image for this read. `VIEW DEFINITION` on each
/// schema answers for a missing grant and for a schema `DENY` alike. An
/// object `DENY` cannot be scoped: its row in `sys.database_permissions`
/// stays visible, through role membership too, but `OBJECT_SCHEMA_NAME` of
/// the hidden object reads NULL, and `HAS_PERMS_BY_NAME` answers 0 for a
/// hidden name and an absent one alike. So any effective object `DENY`
/// refuses, and `HAS_PERMS_BY_NAME` on the object keeps an owner's or
/// sysadmin's override from reading as one.
pub async fn prove_schemas_visible(conn: &mut Conn, schemas: &[String]) -> Result<(), DbError> {
    if schemas.is_empty() {
        return Ok(());
    }
    let rows = schemas
        .iter()
        .map(|s| format!("({})", crate::ident::literal(s)))
        .collect::<Vec<_>>()
        .join(", ");
    // Resolved through `sys.schemas` and quoted: `HAS_PERMS_BY_NAME` parses
    // its argument, so a raw `a.b` answers 0 and `x]y` NULL (measured;
    // review of #1360). A schema the database does not hold has nothing in
    // it to hide, and `HAS_PERMS_BY_NAME` answers 0 for it even to sysadmin,
    // so it is left out rather than reported as a missing grant; a table
    // planned into it fails at apply naming the schema, as it did before.
    let hidden: Vec<String> = conn
        .query(&format!(
            "SELECT s.name AS schema_name FROM (VALUES {rows}) AS w(schema_name)
               JOIN sys.schemas s ON s.name = w.schema_name COLLATE CATALOG_DEFAULT
              WHERE COALESCE(HAS_PERMS_BY_NAME(QUOTENAME(s.name), 'SCHEMA', 'VIEW DEFINITION'), 0) <> 1;"
        ))
        .await?
        .iter()
        .map(|row| get::<&str>(row, "schema_name").map(str::to_owned))
        .collect::<Result<_, _>>()?;
    if !hidden.is_empty() {
        return Err(DbError::Refused(format!(
            "this login does not hold VIEW DEFINITION on schema {}, so objects there can be \
             hidden from sys.objects",
            hidden.join(", ")
        )));
    }
    let denials = conn
        .query(
            "SELECT TOP (1) dp.major_id FROM sys.database_permissions dp
              WHERE dp.class = 1 AND dp.state = N'D'
                AND dp.permission_name IN (N'VIEW DEFINITION', N'CONTROL')
                AND (dp.grantee_principal_id = USER_ID()
                     OR IS_MEMBER(USER_NAME(dp.grantee_principal_id)) = 1)
                AND COALESCE(HAS_PERMS_BY_NAME(
                      QUOTENAME(OBJECT_SCHEMA_NAME(dp.major_id)) + N'.'
                        + QUOTENAME(OBJECT_NAME(dp.major_id)),
                      'OBJECT', 'VIEW DEFINITION'), 0) <> 1;",
        )
        .await?;
    if !denials.is_empty() {
        return Err(DbError::Refused(
            "an object DENY of VIEW DEFINITION or CONTROL hides an object from this login, \
             and the catalog does not say which schema it is in"
                .into(),
        ));
    }
    Ok(())
}

/// Which of `roles`, names a read of `sys.database_principals` did not
/// return, the database holds anyway (#1606). That view is filtered by
/// metadata visibility: an effective `DENY VIEW DEFINITION` or `DENY CONTROL`
/// on a role, to this login or a role it is in, hides the role's row, and a
/// login without database `VIEW DEFINITION` sees only the roles it belongs to.
///
/// DEC-1606.1: `DATABASE_PRINCIPAL_ID` is not filtered (measured on the pinned
/// 17.0 image). It resolves a role under each of those `DENY`s and for a
/// login holding nothing but `CONNECT`, and answers NULL only for a name the
/// database lacks, so it tells hidden from absent by name. A permission check
/// cannot: `ALTER ANY ROLE` alone shows every role while `VIEW DEFINITION` on
/// the database answers 0. A name that resolves to a user rather than a role
/// is returned too: it is not absent, whatever it is.
pub async fn held_principals(conn: &mut Conn, roles: &[String]) -> Result<Vec<String>, DbError> {
    if roles.is_empty() {
        return Ok(Vec::new());
    }
    let rows = roles
        .iter()
        .map(|r| format!("({})", crate::ident::literal(r)))
        .collect::<Vec<_>>()
        .join(", ");
    conn.query(&format!(
        "SELECT w.name FROM (VALUES {rows}) AS w(name)
          WHERE DATABASE_PRINCIPAL_ID(w.name) IS NOT NULL;"
    ))
    .await?
    .iter()
    .map(|row| get::<&str>(row, "name").map(str::to_owned))
    .collect()
}

/// The [`NameOccupant`]s at `names`, and every object whose parent is one of
/// `parents` (its constraints, defaults and triggers), compared under the
/// catalog collation that names them (see [`object_names_alike`]), read in the
/// caller's transaction. An object found
/// only as a child has its own name as `wanted`.
pub async fn object_name_occupants(
    conn: &mut Conn,
    names: &[TableName],
    parents: &[TableName],
) -> Result<Vec<NameOccupant>, DbError> {
    if names.is_empty() && parents.is_empty() {
        return Ok(Vec::new());
    }
    // `VALUES` cannot be empty, so an empty list is a row that matches
    // nothing: `i` of -1 and names no identifier can be.
    let values = |list: &[TableName]| {
        if list.is_empty() {
            return "(-1, CAST(NULL AS sysname), CAST(NULL AS sysname))".to_owned();
        }
        list.iter()
            .enumerate()
            .map(|(i, n)| {
                format!(
                    "({i}, {}, {})",
                    crate::ident::literal(&n.schema),
                    crate::ident::literal(&n.name)
                )
            })
            .collect::<Vec<_>>()
            .join(", ")
    };
    let sql = format!(
        "SELECT w.i, s.name AS schema_name, o.name AS object_name,
                CONVERT(nvarchar(60), o.type_desc) AS type_desc,
                ps.name AS parent_schema, p.name AS parent_name,
                c.name AS parent_column
           FROM sys.objects o
           JOIN sys.schemas s ON s.schema_id = o.schema_id
           LEFT JOIN (VALUES {}) AS w(i, schema_name, object_name)
             ON s.name = w.schema_name COLLATE CATALOG_DEFAULT
            AND o.name = w.object_name COLLATE CATALOG_DEFAULT
           LEFT JOIN sys.objects p ON p.object_id = o.parent_object_id
           LEFT JOIN sys.schemas ps ON ps.schema_id = p.schema_id
           LEFT JOIN sys.default_constraints dc ON dc.object_id = o.object_id
           LEFT JOIN sys.columns c
             ON c.object_id = dc.parent_object_id AND c.column_id = dc.parent_column_id
          WHERE w.i IS NOT NULL
             OR EXISTS (SELECT 1 FROM (VALUES {}) AS q(i, schema_name, table_name)
                         WHERE q.schema_name = ps.name COLLATE CATALOG_DEFAULT
                           AND q.table_name = p.name COLLATE CATALOG_DEFAULT)
          ORDER BY s.name, o.name;",
        values(names),
        values(parents)
    );
    let mut out = Vec::new();
    for row in conn.query(&sql).await? {
        let name = TableName::new(
            get::<&str>(&row, "schema_name")?,
            get::<&str>(&row, "object_name")?,
        );
        let wanted = match row.try_get::<i32>("i")? {
            Some(i) => names.get(i as usize).cloned().ok_or_else(|| {
                DbError::BadRow(format!("the name-occupant query returned index {i}"))
            })?,
            None => name.clone(),
        };
        let parent = match (
            row.try_get::<&str>("parent_schema")?,
            row.try_get::<&str>("parent_name")?,
        ) {
            (Some(schema), Some(name)) => Some(TableName::new(schema, name)),
            _ => None,
        };
        out.push(NameOccupant {
            wanted,
            name,
            kind: get::<&str>(&row, "type_desc")?
                .to_lowercase()
                .replace('_', " "),
            parent,
            parent_column: row.try_get::<&str>("parent_column")?.map(str::to_owned),
        });
    }
    Ok(out)
}

/// Among `names`, the pairs of columns of one group the database reads as one
/// column name under its catalog collation, by index into `names`, each as
/// `(earlier, later)`. A group is one table, by the caller's index. A fold in
/// Rust cannot answer this: on `Turkish_100_CI_AS`, `A` and `a` are one name
/// and `I` and `i` are two (#1366).
pub async fn column_names_alike(
    conn: &mut Conn,
    names: &[(usize, String)],
) -> Result<Vec<(usize, usize)>, DbError> {
    if names.len() < 2 {
        return Ok(Vec::new());
    }
    let values = names
        .iter()
        .enumerate()
        .map(|(k, (group, name))| format!("({k}, {group}, {})", crate::ident::literal(name)))
        .collect::<Vec<_>>()
        .join(", ");
    let sql = format!(
        "SELECT a.k AS earlier, b.k AS later
           FROM (VALUES {values}) AS a(k, g, column_name)
           JOIN (VALUES {values}) AS b(k, g, column_name)
             ON a.k < b.k AND a.g = b.g
            AND a.column_name = b.column_name COLLATE CATALOG_DEFAULT
          ORDER BY a.k, b.k;"
    );
    let mut out = Vec::new();
    for row in conn.query(&sql).await? {
        let index = |column: &str| -> Result<usize, DbError> {
            let k = get::<i32>(&row, column)?;
            usize::try_from(k)
                .ok()
                .filter(|k| *k < names.len())
                .ok_or_else(|| DbError::BadRow(format!("an alike-column index out of range: {k}")))
        };
        out.push((index("earlier")?, index("later")?));
    }
    Ok(out)
}

/// Of the groups in `added`, the ones where an added column's name is a name
/// the same group's `recorded` columns have, compared under the catalog
/// collation (#676, #1243). A group is one table, by the caller's index. Asked of the
/// engine for the reason [`matching_table_names`] is: an accent-, width- or
/// kana-insensitive database reads `café` and `cafe` as one column, which no
/// fold in Rust reproduces.
pub async fn tables_reusing_a_column_name(
    conn: &mut Conn,
    added: &[(usize, String)],
    recorded: &[(usize, String)],
) -> Result<std::collections::BTreeSet<usize>, DbError> {
    if added.is_empty() || recorded.is_empty() {
        return Ok(std::collections::BTreeSet::new());
    }
    let values = |names: &[(usize, String)]| {
        names
            .iter()
            .map(|(i, n)| format!("({i}, {})", crate::ident::literal(n)))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let sql = format!(
        "SELECT DISTINCT a.i FROM (VALUES {}) AS a(i, column_name)
         JOIN (VALUES {}) AS b(i, column_name)
           ON a.i = b.i AND a.column_name = b.column_name COLLATE CATALOG_DEFAULT
         ORDER BY a.i;",
        values(added),
        values(recorded)
    );
    let mut found = std::collections::BTreeSet::new();
    for row in conn.query(&sql).await? {
        found.insert(get::<i32>(&row, "i")? as usize);
    }
    Ok(found)
}

/// Among `names`, the pairs of schema-scoped objects the database reads as one
/// `sys.objects` name: the same schema and the same name under its catalog
/// collation, such as `dbo.Ck_Name` and `dbo.ck_name` on a case-insensitive
/// database. `CATALOG_DEFAULT`, not `DATABASE_DEFAULT`: a partially contained
/// database names its objects under a fixed catalog collation of its own.
/// Measured on 17.0: with an accent-insensitive database collation, `cafe` and
/// `café` are two constraints there and one anywhere else, and the two
/// collations agree on a database that is not contained (review of #1240,
/// DEC-1243.1).
/// Each pair comes once, as `(earlier, later)` in the order given. The same
/// question as [`names_alike`], with the schema taking part (#1215). A fold done
/// in Rust would refuse a valid plan on a case-sensitive database, so it is
/// the database that answers.
pub async fn object_names_alike(
    conn: &mut Conn,
    names: &[TableName],
) -> Result<Vec<(TableName, TableName)>, DbError> {
    if names.len() < 2 {
        return Ok(Vec::new());
    }
    let values = names
        .iter()
        .enumerate()
        .map(|(i, n)| {
            format!(
                "({i}, {}, {})",
                crate::ident::literal(&n.schema),
                crate::ident::literal(&n.name)
            )
        })
        .collect::<Vec<_>>()
        .join(", ");
    let sql = format!(
        "SELECT a.i AS earlier, b.i AS later
           FROM (VALUES {values}) AS a(i, schema_name, object_name)
           JOIN (VALUES {values}) AS b(i, schema_name, object_name)
             ON a.i < b.i
            AND a.schema_name = b.schema_name COLLATE CATALOG_DEFAULT
            AND a.object_name = b.object_name COLLATE CATALOG_DEFAULT
          ORDER BY a.i, b.i;"
    );
    let mut out = Vec::new();
    for row in conn.query(&sql).await? {
        let index = |column: &str| -> Result<usize, DbError> {
            let i = get::<i32>(&row, column)?;
            usize::try_from(i)
                .ok()
                .filter(|i| *i < names.len())
                .ok_or_else(|| DbError::BadRow(format!("an alike-name index out of range: {i}")))
        };
        out.push((
            names[index("earlier")?].clone(),
            names[index("later")?].clone(),
        ));
    }
    Ok(out)
}

/// Among `names`, the pairs the database reads as one name — `Reader` and
/// `reader` under a case-insensitive catalog collation (#1243) — each as
/// `(earlier, later)`
/// in the order given. A plan that creates both passes every check against
/// the catalog, and the second `CREATE ROLE` fails after everything before
/// it has run (DECISIONS 123).
pub async fn names_alike(
    conn: &mut Conn,
    names: &[&str],
) -> Result<Vec<(String, String)>, DbError> {
    if names.len() < 2 {
        return Ok(Vec::new());
    }
    let values = names
        .iter()
        .enumerate()
        .map(|(i, n)| format!("({i}, {})", crate::ident::literal(n)))
        .collect::<Vec<_>>()
        .join(", ");
    // Numbered, so a name is not paired with itself and each pair comes once.
    let sql = format!(
        "SELECT a.name AS earlier, b.name AS later
           FROM (VALUES {values}) AS a(i, name)
           JOIN (VALUES {values}) AS b(i, name)
             ON a.i < b.i AND a.name = b.name COLLATE CATALOG_DEFAULT
          ORDER BY a.i, b.i;"
    );
    let mut out = Vec::new();
    for row in conn.query(&sql).await? {
        out.push((
            get::<&str>(&row, "earlier")?.to_owned(),
            get::<&str>(&row, "later")?.to_owned(),
        ));
    }
    Ok(out)
}

/// The classes a database principal can own: the catalog view that records
/// the owner, and the `SELECT` arm that spells each owned securable the way
/// T-SQL names it (`SCHEMA::sales`, `ROLE::auditors`, `MESSAGE TYPE::m`).
///
/// The list is every catalog view with a `principal_id` or
/// `owning_principal_id` column, read off a live server rather than recalled:
/// the first version of it named the classes that came to mind and missed a
/// role that owns another role, exactly the drop a staged apply would have
/// committed every `DROP MEMBER` for before failing (DECISIONS 88). A view
/// that arrived after SQL Server 2008 is named in the first field and probed
/// for before the query is built, so an older engine answers with the classes
/// it has instead of refusing the whole question.
const OWNABLE: &[(Option<&str>, &str)] = &[
    (
        None,
        "SELECT principal_id, N'SCHEMA::' + name FROM sys.schemas",
    ),
    (
        None,
        "SELECT o.principal_id, N'OBJECT::' + SCHEMA_NAME(o.schema_id) + N'.' + o.name \
         FROM sys.objects o WHERE o.principal_id IS NOT NULL AND o.parent_object_id = 0",
    ),
    (
        None,
        "SELECT t.principal_id, N'TYPE::' + SCHEMA_NAME(t.schema_id) + N'.' + t.name \
         FROM sys.types t WHERE t.principal_id IS NOT NULL",
    ),
    (
        None,
        "SELECT x.principal_id, N'XML SCHEMA COLLECTION::' + SCHEMA_NAME(x.schema_id) + N'.' + \
         x.name FROM sys.xml_schema_collections x WHERE x.principal_id IS NOT NULL",
    ),
    (
        None,
        "SELECT p.owning_principal_id, \
         CASE p.type WHEN 'A' THEN N'APPLICATION ROLE::' ELSE N'ROLE::' END + p.name \
         FROM sys.database_principals p WHERE p.owning_principal_id IS NOT NULL",
    ),
    (
        None,
        "SELECT principal_id, N'ASSEMBLY::' + name FROM sys.assemblies",
    ),
    (
        None,
        "SELECT principal_id, N'CERTIFICATE::' + name FROM sys.certificates",
    ),
    (
        None,
        "SELECT principal_id, N'SYMMETRIC KEY::' + name FROM sys.symmetric_keys",
    ),
    (
        None,
        "SELECT principal_id, N'ASYMMETRIC KEY::' + name FROM sys.asymmetric_keys",
    ),
    (
        None,
        "SELECT principal_id, N'FULLTEXT CATALOG::' + name FROM sys.fulltext_catalogs",
    ),
    (
        None,
        "SELECT principal_id, N'FULLTEXT STOPLIST::' + name FROM sys.fulltext_stoplists",
    ),
    (
        None,
        "SELECT principal_id, N'MESSAGE TYPE::' + name FROM sys.service_message_types",
    ),
    (
        None,
        "SELECT principal_id, N'CONTRACT::' + name FROM sys.service_contracts",
    ),
    (
        None,
        "SELECT principal_id, N'SERVICE::' + name FROM sys.services",
    ),
    (
        None,
        "SELECT principal_id, N'ROUTE::' + name FROM sys.routes",
    ),
    (
        None,
        "SELECT principal_id, N'REMOTE SERVICE BINDING::' + name FROM sys.remote_service_bindings",
    ),
    (
        None,
        "SELECT principal_id, N'EVENT NOTIFICATION::' + name FROM sys.event_notifications",
    ),
    (
        Some("registered_search_property_lists"),
        "SELECT principal_id, N'SEARCH PROPERTY LIST::' + name \
         FROM sys.registered_search_property_lists",
    ),
    (
        Some("database_scoped_credentials"),
        "SELECT principal_id, N'DATABASE SCOPED CREDENTIAL::' + name \
         FROM sys.database_scoped_credentials",
    ),
    (
        Some("external_libraries"),
        "SELECT principal_id, N'EXTERNAL LIBRARY::' + name FROM sys.external_libraries",
    ),
    (
        Some("external_languages"),
        "SELECT principal_id, N'EXTERNAL LANGUAGE::' + language FROM sys.external_languages",
    ),
];

/// The ownership query over the classes whose view `present` says exists.
fn owned_query(present: &dyn Fn(&str) -> bool) -> String {
    // The catalog's name columns do not all share a collation (the
    // `language` of `sys.external_languages` is binary), and a UNION refuses
    // to pick one; the database's own is the right answer for names in it.
    let arms = OWNABLE
        .iter()
        .filter(|(since, _)| since.is_none_or(present))
        .enumerate()
        .map(|(i, (_, arm))| {
            format!(
                "SELECT principal_id, securable COLLATE DATABASE_DEFAULT \
                 FROM ({arm}) AS a{i} (principal_id, securable)"
            )
        })
        .collect::<Vec<_>>()
        .join("\n        UNION ALL\n        ");
    format!(
        "\
SELECT r.name AS role_name, x.securable
  FROM sys.database_principals r
  JOIN (
        {arms}
       ) AS x (principal_id, securable) ON x.principal_id = r.principal_id
 WHERE r.type = 'R' AND r.is_fixed_role = 0 AND r.name <> 'public'
 ORDER BY r.name, x.securable;"
    )
}

/// Every securable each user-defined role **owns**, spelled the way T-SQL
/// names it (`SCHEMA::sales`, `OBJECT::dbo.t`, `ROLE::auditors`, ...), over
/// every class the catalog can assign an owner to ([`OWNABLE`]).
///
/// The engine refuses to drop a role that owns anything, and ownership is
/// each environment's own, like membership. A connected plan reads this so
/// the drop is refused *before* anything runs — a staged apply would
/// otherwise commit every `DROP MEMBER` and then fail on the `DROP ROLE`,
/// leaving users without access and the role still there.
pub async fn role_owned_securables(
    conn: &mut Conn,
) -> Result<BTreeMap<String, Vec<String>>, DbError> {
    // The views that are not on every engine, asked about in one round trip:
    // `OBJECT_ID` is NULL for a view this version does not have.
    let optional: Vec<&str> = OWNABLE.iter().filter_map(|(since, _)| *since).collect();
    let probe = format!(
        "SELECT {};",
        optional
            .iter()
            .map(|view| format!("OBJECT_ID(N'sys.{view}')"))
            .collect::<Vec<_>>()
            .join(", ")
    );
    let mut present = Vec::with_capacity(optional.len());
    let probed = conn.query(&probe).await?;
    let Some(row) = probed.first() else {
        return Err(DbError::BadRow(
            "the catalog probe returned no row".to_owned(),
        ));
    };
    for (i, view) in optional.iter().enumerate() {
        if row.try_get_at::<i32>(i)?.is_some() {
            present.push(*view);
        }
    }
    let query = owned_query(&|view| present.contains(&view));
    let mut out: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for row in conn.query(&query).await? {
        out.entry(get::<&str>(&row, "role_name")?.to_owned())
            .or_default()
            .push(get::<&str>(&row, "securable")?.to_owned());
    }
    Ok(out)
}

pub async fn role_members(conn: &mut Conn) -> Result<BTreeMap<String, Vec<String>>, DbError> {
    const MEMBERS: &str = "\
SELECT r.name AS role_name, m.name AS member_name
  FROM sys.database_role_members rm
  JOIN sys.database_principals r ON r.principal_id = rm.role_principal_id
  JOIN sys.database_principals m ON m.principal_id = rm.member_principal_id
 WHERE r.type = 'R' AND r.is_fixed_role = 0 AND r.name <> 'public'
 ORDER BY r.name, m.name;";
    let mut out: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for row in conn.query(MEMBERS).await? {
        out.entry(get::<&str>(&row, "role_name")?.to_owned())
            .or_default()
            .push(get::<&str>(&row, "member_name")?.to_owned());
    }
    Ok(out)
}

/// Reads the rows of every scoped table the schema has (ADR-0004).
///
/// The scope decides *which* rows — every row of an `exact` table, the
/// declared keys of an `ensure` one — and it is supplied by the caller, because
/// a database holds rows, not a notion of which of them are declared. A scoped
/// table the schema does not have gets no entry: it is missing, which the
/// managed-set check reports, and "missing" must not come back as "empty".
///
/// A table whose rows cannot be read (no single-column key, or a value the
/// model cannot hold) fails the whole read rather than being skipped: a state
/// recorded without it would say the table declares no rows, and the next
/// drift check would be blind to the rows it exists to watch.
pub async fn read_rows(
    conn: &mut Conn,
    schema: &Schema,
    scopes: &BTreeMap<TableName, RowScope>,
) -> Result<ObservedRows, crate::rows::RowsError> {
    let mut out = ObservedRows::new();
    for (name, scope) in scopes {
        let Some(table) = schema.tables.get(name) else {
            continue;
        };
        let mut observed = pbps_model::ObservedTable::default();
        if let Some(query) = crate::rows::query(name, table, scope)? {
            let read = |source| crate::rows::RowsError::Read {
                table: name.clone(),
                source: Box::new(source),
            };
            let result = conn.query(&query.sql).await.map_err(read)?;
            for row in &result {
                let (key, cells) = crate::rows::decode(name, &query, row)?;
                observed.rows.insert(key, cells);
            }
            if let Some(sql) = &query.aliases {
                for row in &conn.query(sql).await.map_err(read)? {
                    let (requested, canonical) = crate::rows::decode_alias(name, row)?;
                    observed.aliases.insert(requested, canonical);
                }
            }
        }
        out.insert(name.clone(), observed);
    }
    Ok(out)
}

/// How the database spells each of the schema names a declaration grants on:
/// `None` where it has no schema of that name at all.
///
/// A schema has no identity to be matched by (ADR-0002); a grant target names
/// it as text, and the text is all the differ has. So a declaration that
/// writes `schema::DBO` where the database says `dbo` is caught by nothing
/// else: on a case-insensitive database the `GRANT` succeeds, introspection
/// reads `dbo` back, and every plan from then on revokes one spelling and
/// grants the other without ever converging (DECISIONS 142).
///
/// Absent is left to the caller and to the pre-flight probe, and is not the
/// same answer as differently spelt: one says create it, the other says write
/// it the way the database already does.
pub async fn schema_spellings(
    conn: &mut Conn,
    names: &std::collections::BTreeSet<String>,
) -> Result<BTreeMap<String, Option<String>>, DbError> {
    let mut out = BTreeMap::new();
    if names.is_empty() {
        return Ok(out);
    }
    let wanted: Vec<&String> = names.iter().collect();
    // Asked by index, never by name: the answer is the database's spelling,
    // and matching it back to the requested one by name would be the very
    // comparison in question.
    let values: Vec<String> = wanted
        .iter()
        .enumerate()
        .map(|(i, n)| format!("({i}, {})", crate::ident::literal(n)))
        .collect();
    let sql = format!(
        "SELECT v.i, SCHEMA_NAME(SCHEMA_ID(v.n)) AS spelled FROM (VALUES {}) AS v(i, n);",
        values.join(", ")
    );
    for row in &conn.query(&sql).await? {
        let i: i32 = row.try_get_at(0)?.ok_or_else(|| {
            DbError::BadRow("the schema spelling query returned a NULL index".to_owned())
        })?;
        let spelled: Option<&str> = row.try_get_at(1)?;
        let Some(name) = usize::try_from(i).ok().and_then(|i| wanted.get(i)) else {
            return Err(DbError::BadRow(format!(
                "the schema spelling query returned index {i} for {} name(s)",
                wanted.len()
            )));
        };
        out.insert((*name).clone(), spelled.map(ToOwned::to_owned));
    }
    Ok(out)
}

pub use pbps_db::catalog::Spellings;

/// Every declared spelling the engine would not read back as written, and
/// every pair of keys it reads as one, over every table that declares rows
/// (DECISIONS 101, 106). Asked of the engine, not of the table: a table this
/// plan creates can be asked too.
///
/// `_at` is the names the catalog has now, which the PostgreSQL side needs.
/// This one needs none since #1175: the only question it asked the catalog was
/// a key column's collation, and the declaration now says what it will be.
pub async fn misspelt(
    conn: &mut Conn,
    schema: &Schema,
    _at: &crate::rows::CatalogNames,
) -> Result<Spellings, crate::rows::RowsError> {
    let mut out = Spellings::default();
    for (name, table) in &schema.tables {
        for q in crate::rows::spelling_queries(name, table)? {
            let read = |source| crate::rows::RowsError::Read {
                table: name.clone(),
                source: Box::new(source),
            };
            if let Some(sql) = &q.collisions {
                for row in &conn.query(sql).await.map_err(read)? {
                    let (a, b, canonical) = crate::rows::decode_collision(name, row)?;
                    let (Some((first, _)), Some((second, _))) =
                        (q.literals.get(a), q.literals.get(b))
                    else {
                        return Err(read(pbps_db::DbError::BadRow(format!(
                            "the collision query returned indexes {a} and {b} for {} literal(s)",
                            q.literals.len()
                        ))));
                    };
                    out.conflicts.push(pbps_model::RowConflict {
                        table: name.clone(),
                        first: first.clone(),
                        second: second.clone(),
                        canonical: pbps_model::RowKey::from(canonical.as_str()),
                    });
                }
            }
            for row in &conn.query(&q.sql).await.map_err(read)? {
                let (i, canonical) = crate::rows::decode_spelling(name, row)?;
                let Some((key, declared)) = q.literals.get(i) else {
                    return Err(read(pbps_db::DbError::BadRow(format!(
                        "the spelling query returned index {i} for {} literal(s)",
                        q.literals.len()
                    ))));
                };
                // A key's spelling is aliased at read time (71); only a text
                // the type cannot read at all is wrong there.
                let agrees = match (&q.column, &canonical) {
                    (_, None) => false,
                    (None, Some(_)) => true,
                    (Some(_), Some(c)) => c == declared,
                };
                if !agrees {
                    out.misspelt.push(crate::rows::Misspelt {
                        table: name.clone(),
                        key: key.clone(),
                        column: q.column.clone(),
                        declared: declared.clone(),
                        ty: q.ty.clone(),
                        canonical,
                    });
                }
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A dependency read that fails is a failed read (#1644): its `CATCH`
    /// raises the error again, and with `RAISERROR`, since `THROW` does not
    /// parse on SQL Server 2008, which the pull still reads.
    #[test]
    fn the_dependency_read_raises_what_it_catches_on_every_server() {
        for sql in [
            MODULE_DEPENDENCIES,
            crate::impact::DEPENDENCIES_TABLE,
            crate::impact::DEPENDENCIES_COLUMN,
        ] {
            assert!(sql.starts_with("BEGIN TRY\n"), "{sql}");
            assert!(
                sql.contains("BEGIN CATCH\n    DECLARE @error nvarchar(2048) = ERROR_MESSAGE();\n    RAISERROR(N'%s', 16, 1, @error);\nEND CATCH;"),
                "{sql}"
            );
            assert!(!sql.contains("THROW"), "{sql}");
        }
    }

    #[test]
    fn old_servers_are_not_asked_for_a_temporal_catalog_column() {
        for version in ["10.50.6000.34", "11.0.7001.0", "12.0.6024.0"] {
            let query = tables_query(version, "Developer Edition", false, false);
            assert!(!query.contains("t.temporal_type"), "{query}");
            assert!(!query.contains("sys.periods"), "{query}");
            assert!(query.contains("CONVERT(tinyint, 0) AS temporal_type"));
            assert!(query.contains("CONVERT(bit, 0) AS has_period"));
            // Nor for any column the history and period reads add (#1176).
            for column in ["history_table_id", "start_column_id", "is_hidden"] {
                assert!(!query.contains(&format!("t.{column}")), "{query}");
            }
            let columns = columns_query(has_temporal(version, "Developer Edition"));
            assert!(!columns.contains("c.generated_always_type"), "{columns}");
            assert!(!columns.contains("c.is_hidden"), "{columns}");
        }
        for (version, edition) in [
            ("13.0.1601.5", "Developer Edition"),
            ("17.0.4075.5", "Developer Edition"),
            ("12.0.2000.8", "SQL Azure"),
            ("unknown", "Developer Edition"),
        ] {
            for has_ledger in [false, true] {
                let query = tables_query(version, edition, has_ledger, has_ledger);
                assert!(query.contains("t.temporal_type"));
                assert!(query.contains("sys.periods"));
                assert!(query.contains("t.history_table_id"));
            }
            let columns = columns_query(has_temporal(version, edition));
            assert!(columns.contains("c.generated_always_type"), "{columns}");
            assert!(columns.contains("c.is_hidden"), "{columns}");
        }
    }

    /// Retention arrived in 2017: a 2016 server is not asked for it, and
    /// reads as INFINITE, which is what it keeps (#1176).
    #[test]
    fn only_a_server_with_history_retention_is_asked_for_it() {
        let query = tables_query("13.0.1601.5", "Developer Edition", false, false);
        assert!(!query.contains("t.history_retention_period"), "{query}");
        assert!(
            query.contains("CONVERT(int, -1) AS retention_period"),
            "{query}"
        );
        for has_ledger in [false, true] {
            let query = tables_query("14.0.1000.169", "Developer Edition", has_ledger, true);
            assert!(query.contains("t.history_retention_period AS"), "{query}");
            assert!(
                query.contains("t.history_retention_period_unit AS"),
                "{query}"
            );
            assert!(!query.contains("{"), "an unfilled placeholder: {query}");
        }
    }

    #[test]
    fn only_a_server_whose_catalog_has_the_ledger_columns_is_asked_for_them() {
        // Azure SQL Edge's banner says Azure on a 15.x engine whose
        // `sys.tables` has no ledger columns; the probe, not the banner, decides.
        for (version, edition) in [
            ("11.0.7001.0", "Developer Edition"),
            ("13.0.1601.5", "Developer Edition"),
            ("15.0.4355.3", "Developer Edition"),
            ("15.0.2000.1574", "Azure SQL Edge Developer"),
            ("12.0.2000.8", "SQL Azure"),
            ("unknown", "Developer Edition"),
        ] {
            let query = tables_query(version, edition, false, false);
            for column in [
                "t.ledger_type",
                "t.is_dropped_ledger_table",
                "t.ledger_view_id",
            ] {
                assert!(!query.contains(column), "{version} {edition}: {query}");
            }
            assert!(query.contains("CONVERT(tinyint, 0) AS ledger_type"));
        }
        for (version, edition) in [
            ("16.0.1000.6", "Developer Edition"),
            ("17.0.4075.5", "Enterprise Developer Edition (64-bit)"),
            ("12.0.2000.8", "SQL Azure"),
        ] {
            let query = tables_query(version, edition, true, true);
            assert!(query.contains("t.ledger_type"), "{version}");
            assert!(query.contains("t.is_dropped_ledger_table"), "{version}");
            assert!(query.contains("t.ledger_view_id"), "{version}");
        }
    }

    #[test]
    fn views_inline_table_functions_and_schema_bound_modules_require_bound_references() {
        for code in ["V", "IF"] {
            assert!(requires_bound_references(code, false), "{code}");
        }
        for code in ["P", "PC", "FN", "TF", "FS", "FT", "TR"] {
            assert!(!requires_bound_references(code, false), "{code}");
        }
        for code in ["FN", "TF"] {
            assert!(requires_bound_references(code, true), "{code}");
        }
    }

    /// An engine without a view answers with the classes it has; one with
    /// every view is asked about every class.
    #[test]
    fn an_absent_catalog_view_removes_its_class_and_nothing_else() {
        let all = owned_query(&|_| true);
        let none = owned_query(&|_| false);
        for (since, arm) in OWNABLE {
            assert!(all.contains(arm), "{arm}");
            assert_eq!(none.contains(arm), since.is_none(), "{arm}");
        }
        // The class that was missed once is not an optional one.
        assert!(none.contains("owning_principal_id"));
        assert!(!none.contains("sys.external_languages"));
        assert!(all.contains("sys.external_languages"));
        // A well-formed derived table: one `UNION ALL` fewer than arms.
        assert_eq!(all.matches("UNION ALL").count() + 1, OWNABLE.len());
    }

    /// The table filter names the ledger's own two tables *qualified*, and
    /// nothing else: a pattern here hid a project's `dbo.__pbps_customers`, and
    /// the bare names still hid its `app.__pbps_state` — both reported a table
    /// that is there as absent (issue #170). Asking [`pbps_db::ledger`] for the
    /// names rather than repeating them keeps a move of the ledger from leaving
    /// the filter behind.
    #[test]
    fn the_table_filter_names_the_ledgers_own_qualified_tables_and_matches_no_pattern() {
        for qualified in [crate::state::STATE_TABLE, crate::state::LOCK_TABLE] {
            let (schema, name) = qualified.split_once('.').expect("a qualified name");
            assert!(
                TABLES.contains(&format!(
                    "(s.name + N'|') COLLATE Latin1_General_BIN2 = N'{schema}|'"
                )),
                "the schema is part of what this tool owns: {qualified}"
            );
            assert!(TABLES.contains(&format!("N'{name}|'")), "{qualified}");
        }
        // Both filters compare the spelling exactly, as `is_ours` reserves it.
        for query in [TABLES, LEGACY_TABLES] {
            assert!(
                query.contains("(t.name + N'|') COLLATE Latin1_General_BIN2"),
                "a collation-dependent filter hides a project's differently cased table"
            );
        }
        assert!(
            !TABLES.contains("LIKE"),
            "a pattern hides more than the two tables it is for"
        );
    }
}
