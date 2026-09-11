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
/// The PostgreSQL pull lists the same two names unqualified, because the schema
/// its ledger will live in is not decided until Phase 5 step 8 (#185).
const TABLES: &str = "\
SELECT t.object_id, s.name AS schema_name, t.name AS table_name, t.temporal_type,
       CONVERT(bit, CASE WHEN p.object_id IS NULL THEN 0 ELSE 1 END) AS has_period
  FROM sys.tables t
  JOIN sys.schemas s ON s.schema_id = t.schema_id
  LEFT JOIN sys.periods p ON p.object_id = t.object_id
 WHERE t.is_ms_shipped = 0
   AND NOT (s.name = 'dbo' AND t.name IN ('__pbps_state', '__pbps_lock'))
 ORDER BY s.name, t.name;";

// SQL Server added both `sys.tables.temporal_type` and `sys.periods` in 2016.
// Keep the whole legacy query free of those names: replacing only the selected
// column would still make an older server compile a join to a view it lacks.
const LEGACY_TABLES: &str = "\
SELECT t.object_id, s.name AS schema_name, t.name AS table_name,
       CONVERT(tinyint, 0) AS temporal_type, CONVERT(bit, 0) AS has_period
  FROM sys.tables t
  JOIN sys.schemas s ON s.schema_id = t.schema_id
 WHERE t.is_ms_shipped = 0
   AND NOT (s.name = 'dbo' AND t.name IN ('__pbps_state', '__pbps_lock'))
 ORDER BY s.name, t.name;";

fn tables_query(product_version: &str, edition: &str) -> String {
    let major = product_version
        .split('.')
        .next()
        .and_then(|v| v.parse::<u32>().ok());
    if !edition.to_ascii_lowercase().contains("azure") && major.is_some_and(|v| v < 13) {
        LEGACY_TABLES.to_owned()
    } else {
        TABLES.to_owned()
    }
}

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
SELECT cc.parent_object_id AS object_id, cc.object_id AS constraint_object_id,
       cc.name, cc.definition
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
SELECT i.object_id, i.name, i.is_unique, i.type AS index_type,
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
const MODULE_DEPENDENCIES: &str = "\
SELECT DISTINCT d.referencing_id, d.referenced_id
  FROM sys.sql_expression_dependencies d
 WHERE d.referenced_id IS NOT NULL
 ORDER BY d.referencing_id, d.referenced_id;";

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
    let versions = conn
        .query(
            "SELECT CONVERT(nvarchar(128), SERVERPROPERTY('ProductVersion')) AS version,
                CONVERT(nvarchar(128), SERVERPROPERTY('Edition')) AS edition;",
        )
        .await?;
    let version = versions
        .first()
        .ok_or_else(|| DbError::BadRow("the server version query returned no row".into()))?;
    let tables = tables_query(get(version, "version")?, get(version, "edition")?);
    for row in conn.query(&tables).await? {
        raw.tables.push(RawTable {
            object_id: get(&row, "object_id")?,
            schema: get::<&str>(&row, "schema_name")?.to_owned(),
            name: get::<&str>(&row, "table_name")?.to_owned(),
            temporal_type: get(&row, "temporal_type")?,
            has_period: get(&row, "has_period")?,
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
            constraint_object_id: get(&row, "constraint_object_id")?,
            name: get::<&str>(&row, "name")?.to_owned(),
            definition: get::<&str>(&row, "definition")?.to_owned(),
        });
    }

    for row in conn.query(INDEX_COLUMNS).await? {
        raw.index_columns.push(RawIndexColumn {
            object_id: get(&row, "object_id")?,
            index_name: get::<&str>(&row, "name")?.to_owned(),
            is_unique: get(&row, "is_unique")?,
            kind: IndexKind::from_type_code(get::<u8>(&row, "index_type")?),
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
/// Asked of the engine under the database collation rather than compared
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
             ON p.name = d.name COLLATE DATABASE_DEFAULT",
        values(names)
    );
    if !except.is_empty() {
        sql.push_str(&format!(
            "
 WHERE NOT EXISTS (SELECT 1 FROM (VALUES {}) AS v(name)
                                WHERE v.name = p.name COLLATE DATABASE_DEFAULT)",
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

/// Among `names`, the pairs the database reads as one name — `Reader` and
/// `reader` under a case-insensitive collation — each as `(earlier, later)`
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
             ON a.i < b.i AND a.name = b.name COLLATE DATABASE_DEFAULT
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
pub async fn misspelt(
    conn: &mut Conn,
    schema: &Schema,
    at: &crate::rows::CatalogNames,
) -> Result<Spellings, crate::rows::RowsError> {
    let mut out = Spellings::default();
    let as_declared = crate::rows::Catalogued::default();
    for (name, table) in &schema.tables {
        let at = at.get(name).unwrap_or(&as_declared);
        for q in crate::rows::spelling_queries(name, table, at)? {
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

    #[test]
    fn old_servers_are_not_asked_for_a_temporal_catalog_column() {
        for version in ["10.50.6000.34", "11.0.7001.0", "12.0.6024.0"] {
            let query = tables_query(version, "Developer Edition");
            assert!(!query.contains("t.temporal_type"), "{query}");
            assert!(!query.contains("sys.periods"), "{query}");
            assert!(query.contains("CONVERT(tinyint, 0) AS temporal_type"));
            assert!(query.contains("CONVERT(bit, 0) AS has_period"));
        }
        for (version, edition) in [
            ("13.0.1601.5", "Developer Edition"),
            ("17.0.4075.5", "Developer Edition"),
            ("12.0.2000.8", "SQL Azure"),
            ("unknown", "Developer Edition"),
        ] {
            let query = tables_query(version, edition);
            assert!(query.contains("t.temporal_type"));
            assert!(query.contains("sys.periods"));
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
                TABLES.contains(&format!("s.name = '{schema}'")),
                "the schema is part of what this tool owns: {qualified}"
            );
            assert!(TABLES.contains(&format!("'{name}'")), "{qualified}");
        }
        assert!(
            !TABLES.contains("LIKE"),
            "a pattern hides more than the two tables it is for"
        );
    }
}
