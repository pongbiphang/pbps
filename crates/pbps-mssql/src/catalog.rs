//! The catalog queries behind [`crate::introspect`].
//!
//! Only this file runs SQL against a live server; it converts `sys.*` rows into
//! the plain `Raw*` structs and hands them to the pure assembler. The queries
//! are static — nothing user-controlled is ever interpolated into them.

use std::collections::BTreeMap;

use pbps_db::{Conn, DbError, FromColumn, Row};
use pbps_model::{ObservedRows, RowScope, Schema, TableName};

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
       ps.name AS parent_schema, pt.name AS parent_table,
       -- Persisted with the module and re-applied on every execution, so they
       -- are part of what it does. NULL for a module with no readable
       -- definition, which is refused for its own reason first.
       CONVERT(bit, ISNULL(m.uses_quoted_identifier, 1)) AS quoted_identifier,
       CONVERT(bit, ISNULL(m.uses_ansi_nulls, 1)) AS ansi_nulls
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
const PERMISSIONS: &str = "\
SELECT pr.name AS role_name, dp.class, dp.class_desc, dp.permission_name, dp.state, dp.minor_id,
       COALESCE(os.name, ss.name) AS schema_name, o.name AS object_name
  FROM sys.database_permissions dp
  JOIN sys.database_principals pr ON pr.principal_id = dp.grantee_principal_id
  LEFT JOIN sys.objects o ON dp.class = 1 AND o.object_id = dp.major_id
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
        let quoted: bool = get(&row, "quoted_identifier")?;
        let ansi_nulls: bool = get(&row, "ansi_nulls")?;
        raw.modules.push(RawModule {
            default_set_options: quoted && ansi_nulls,
            schema: get::<&str>(&row, "schema_name")?.to_owned(),
            name: get::<&str>(&row, "object_name")?.to_owned(),
            kind,
            definition: opt::<&str>(&row, "definition")?.map(str::to_owned),
            parent,
        });
    }

    for row in conn.query(ROLES).await? {
        raw.roles.push(crate::introspect::RawRole {
            name: get::<&str>(&row, "name")?.to_owned(),
        });
    }

    for row in conn.query(PERMISSIONS).await? {
        let class: u8 = get(&row, "class")?;
        // A permission on an object the catalog has no schema for (a dropped
        // object's orphaned row) has nothing to be declared against. Any
        // other class has no schema to begin with and is carried as is.
        let schema = match opt::<&str>(&row, "schema_name")? {
            Some(schema) => schema.to_owned(),
            None if matches!(class, 1 | 3) => continue,
            None => String::new(),
        };
        raw.permissions.push(crate::introspect::RawPermission {
            role: get::<&str>(&row, "role_name")?.to_owned(),
            class,
            class_desc: get::<&str>(&row, "class_desc")?.trim().to_owned(),
            permission: get::<&str>(&row, "permission_name")?.trim().to_owned(),
            state: get::<&str>(&row, "state")?.to_owned(),
            schema,
            object: opt::<&str>(&row, "object_name")?.map(str::to_owned),
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

/// What the engine says about the declared spellings of every table that
/// declares rows: the ones it would not read back as written, and the keys
/// it reads as one row.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Spellings {
    pub misspelt: Vec<crate::rows::Misspelt>,
    pub conflicts: Vec<pbps_model::RowConflict>,
}

/// Every declared spelling the engine would not read back as written, and
/// every pair of keys it reads as one, over every table that declares rows
/// (DECISIONS 101, 106). Asked of the engine, not of the table: a table this
/// plan creates can be asked too.
pub async fn misspelt(
    conn: &mut Conn,
    schema: &Schema,
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
}
