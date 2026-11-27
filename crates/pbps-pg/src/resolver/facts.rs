//! Connected catalog reads supplying [`DependencyFacts`] for PostgreSQL
//! (DEC-2987.1).
//!
//! Runs in one `REPEATABLE READ READ ONLY` snapshot under an empty
//! `search_path`. Every catalog reference is `pg_catalog.`-qualified and every
//! input list rides through `pg_catalog.jsonb_to_recordset` (static SQL).

use std::collections::{BTreeMap, BTreeSet};

use pbps_db::{Conn, DbError, FromColumn, Param, Row};
use pbps_diff::resolver::FactsNeeded;
use pbps_model::resolver::{
    Ancestor, CatalogName, DependencyEdge, DependencyFacts, NameAvailability, ReadFact, Surface,
    SurfaceDependencies,
};
use pbps_model::{ModuleId, ObjectName, TableName};
use serde::Serialize;

const CANONICAL_PATH: &str = "SELECT pg_catalog.set_config('search_path', '', true) AS was_set";

const SURFACES_SQL: &str = r#"
WITH req AS (
    SELECT idx, kind, schema_name, obj_name, sub_name
      FROM pg_catalog.jsonb_to_recordset($1::pg_catalog.text::pg_catalog.jsonb)
        AS r(idx int4, kind text, schema_name text, obj_name text, sub_name text)
),
resolved AS (
    SELECT req.idx, req.kind, c.oid AS root_oid, true AS parsed,
           c.relnamespace AS self_nsp, c.oid AS self_rel, 0::int2 AS self_attnum,
           0::pg_catalog.oid AS self_lang,
           'pg_catalog.pg_class'::pg_catalog.regclass AS dep_classid, c.oid AS dep_objid,
           false AS include_auto, rw.oid AS extra_oid
      FROM req
      JOIN pg_catalog.pg_namespace n ON n.nspname = req.schema_name
      JOIN pg_catalog.pg_class c ON c.relnamespace = n.oid AND c.relname = req.obj_name AND c.relkind = 'v'
      JOIN pg_catalog.pg_rewrite rw ON rw.ev_class = c.oid AND rw.rulename = '_RETURN'
     WHERE req.kind = 'view'
    UNION ALL
    SELECT req.idx, req.kind, p.oid, (p.prosqlbody IS NOT NULL),
           p.pronamespace, 0::pg_catalog.oid, 0::int2, p.prolang,
           'pg_catalog.pg_proc'::pg_catalog.regclass, p.oid, false, 0::pg_catalog.oid
      FROM req
      JOIN pg_catalog.pg_namespace n ON n.nspname = req.schema_name
      JOIN pg_catalog.pg_proc p ON p.pronamespace = n.oid AND p.proname = req.obj_name AND p.prokind IN ('f', 'p')
     WHERE req.kind = 'routine'
       AND COALESCE((SELECT pg_catalog.string_agg(pg_catalog.format_type(u.ty, NULL), ',' ORDER BY u.pos)
                       FROM pg_catalog.unnest(p.proargtypes) WITH ORDINALITY AS u(ty, pos)), '') = req.sub_name
    UNION ALL
    SELECT req.idx, req.kind, tg.oid, true, c.relnamespace, tg.tgrelid, 0::int2, 0::pg_catalog.oid,
           'pg_catalog.pg_trigger'::pg_catalog.regclass, tg.oid, true, 0::pg_catalog.oid
      FROM req
      JOIN pg_catalog.pg_namespace n ON n.nspname = req.schema_name
      JOIN pg_catalog.pg_class c ON c.relnamespace = n.oid AND c.relname = req.obj_name
      JOIN pg_catalog.pg_trigger tg ON tg.tgrelid = c.oid AND tg.tgname = req.sub_name AND NOT tg.tgisinternal
     WHERE req.kind = 'trigger'
    UNION ALL
    SELECT req.idx, req.kind, ad.oid, true, c.relnamespace, ad.adrelid, ad.adnum, 0::pg_catalog.oid,
           'pg_catalog.pg_attrdef'::pg_catalog.regclass, ad.oid, true, 0::pg_catalog.oid
      FROM req
      JOIN pg_catalog.pg_namespace n ON n.nspname = req.schema_name
      JOIN pg_catalog.pg_class c ON c.relnamespace = n.oid AND c.relname = req.obj_name
      JOIN pg_catalog.pg_attribute a ON a.attrelid = c.oid AND a.attname = req.sub_name AND a.attnum > 0 AND NOT a.attisdropped
      JOIN pg_catalog.pg_attrdef ad ON ad.adrelid = c.oid AND ad.adnum = a.attnum
     WHERE req.kind = 'default'
    UNION ALL
    SELECT req.idx, req.kind, con.oid, true, con.connamespace, con.conrelid, 0::int2, 0::pg_catalog.oid,
           'pg_catalog.pg_constraint'::pg_catalog.regclass, con.oid, true, 0::pg_catalog.oid
      FROM req
      JOIN pg_catalog.pg_namespace n ON n.nspname = req.schema_name
      JOIN pg_catalog.pg_class c ON c.relnamespace = n.oid AND c.relname = req.obj_name
      JOIN pg_catalog.pg_constraint con ON con.conrelid = c.oid AND con.conname = req.sub_name AND con.contype = 'c'
     WHERE req.kind = 'check'
    UNION ALL
    SELECT req.idx, req.kind, idx.oid, true, idx.relnamespace, i.indrelid, 0::int2, 0::pg_catalog.oid,
           'pg_catalog.pg_class'::pg_catalog.regclass, idx.oid, true, 0::pg_catalog.oid
      FROM req
      JOIN pg_catalog.pg_namespace n ON n.nspname = req.schema_name
      JOIN pg_catalog.pg_class tbl ON tbl.relnamespace = n.oid AND tbl.relname = req.obj_name
      JOIN pg_catalog.pg_index i ON i.indrelid = tbl.oid
      JOIN pg_catalog.pg_class idx ON idx.oid = i.indexrelid AND idx.relname = req.sub_name
     WHERE req.kind = 'index'
),
owned AS (
    SELECT idx, kind, root_oid, parsed, self_nsp, self_rel, self_attnum, self_lang,
           dep_classid, dep_objid, include_auto FROM resolved
    UNION ALL
    SELECT idx, kind, root_oid, parsed, self_nsp, self_rel, self_attnum, self_lang,
           'pg_catalog.pg_rewrite'::pg_catalog.regclass, extra_oid, false
      FROM resolved WHERE kind = 'view'
),
mapped AS (
    SELECT o.idx, o.root_oid::int8 AS root_oid, o.parsed,
           (d.refclassid IS NOT NULL) AS has_edge,
           d.refobjid::int8 AS ref_oid,
           (
             (d.refclassid = 'pg_catalog.pg_namespace'::pg_catalog.regclass AND d.refobjid = o.self_nsp)
             OR (o.kind = 'routine' AND d.refclassid = 'pg_catalog.pg_language'::pg_catalog.regclass AND d.refobjid = o.self_lang)
             OR (o.kind = 'view' AND d.refclassid = 'pg_catalog.pg_class'::pg_catalog.regclass AND d.refobjid = o.self_rel)
             OR (o.kind = 'default' AND d.refclassid = 'pg_catalog.pg_class'::pg_catalog.regclass AND d.refobjid = o.self_rel AND d.refobjsubid = o.self_attnum)
           ) AS is_self_structural,
           t.target_kind, t.target_schema, t.target_name, t.target_sub,
           t.extra_schema, t.extra_name, t.target_nsp
      FROM owned o
      LEFT JOIN pg_catalog.pg_depend d
        ON d.classid = o.dep_classid AND d.objid = o.dep_objid
       AND (d.deptype = 'n' OR (o.include_auto AND d.deptype = 'a'))
      LEFT JOIN LATERAL (
          SELECT CASE WHEN d.refobjsubid > 0 THEN 'column' WHEN d.refobjsubid < 0 THEN 'unknown'
                      WHEN tc.relkind = 'c' THEN 'composite_rel' ELSE 'relation' END::text AS target_kind,
                 tn.nspname::text AS target_schema, tc.relname::text AS target_name, ta.attname::text AS target_sub,
                 NULL::text AS extra_schema, NULL::text AS extra_name, tc.relnamespace AS target_nsp
            FROM pg_catalog.pg_class tc
            JOIN pg_catalog.pg_namespace tn ON tn.oid = tc.relnamespace
            LEFT JOIN pg_catalog.pg_attribute ta
              ON d.refobjsubid > 0 AND ta.attrelid = tc.oid AND ta.attnum = d.refobjsubid AND NOT ta.attisdropped
           WHERE d.refclassid = 'pg_catalog.pg_class'::pg_catalog.regclass AND tc.oid = d.refobjid
          UNION ALL
          SELECT CASE WHEN trel.oid IS NOT NULL AND trel.relkind <> 'c' THEN 'row_type' ELSE 'type' END::text,
                 CASE WHEN trel.oid IS NOT NULL AND trel.relkind <> 'c' THEN treln.nspname ELSE ten.nspname END::text,
                 CASE WHEN trel.oid IS NOT NULL AND trel.relkind <> 'c' THEN trel.relname ELSE te.typname END::text,
                 NULL::text, NULL::text, NULL::text,
                 CASE WHEN trel.oid IS NOT NULL AND trel.relkind <> 'c' THEN trel.relnamespace ELSE te.typnamespace END
            FROM pg_catalog.pg_type tt
            LEFT JOIN pg_catalog.pg_type tel ON tel.oid = tt.typelem AND tel.typarray = tt.oid
            JOIN pg_catalog.pg_type te ON te.oid = COALESCE(tel.oid, tt.oid)
            JOIN pg_catalog.pg_namespace ten ON ten.oid = te.typnamespace
            LEFT JOIN pg_catalog.pg_class trel ON te.typtype = 'c' AND te.typrelid <> 0 AND trel.oid = te.typrelid
            LEFT JOIN pg_catalog.pg_namespace treln ON treln.oid = trel.relnamespace
           WHERE d.refclassid = 'pg_catalog.pg_type'::pg_catalog.regclass AND tt.oid = d.refobjid
          UNION ALL
          -- An array type is also a name of its own: written as a function
          -- (`_mood(x)`), it is a cast until a routine of that name arrives.
          SELECT 'type'::text, an.nspname::text, at.typname::text, NULL::text, NULL::text, NULL::text, at.typnamespace
            FROM pg_catalog.pg_type at
            JOIN pg_catalog.pg_type ae ON ae.oid = at.typelem AND ae.typarray = at.oid
            JOIN pg_catalog.pg_namespace an ON an.oid = at.typnamespace
           WHERE d.refclassid = 'pg_catalog.pg_type'::pg_catalog.regclass AND at.oid = d.refobjid
          UNION ALL
          SELECT 'routine'::text, n.nspname::text, p.proname::text, NULL::text, NULL::text, NULL::text, p.pronamespace
            FROM pg_catalog.pg_proc p JOIN pg_catalog.pg_namespace n ON n.oid = p.pronamespace
           WHERE d.refclassid = 'pg_catalog.pg_proc'::pg_catalog.regclass AND p.oid = d.refobjid
          UNION ALL
          SELECT 'operator'::text, n.nspname::text, op.oprname::text, NULL::text, NULL::text, NULL::text, op.oprnamespace
            FROM pg_catalog.pg_operator op JOIN pg_catalog.pg_namespace n ON n.oid = op.oprnamespace
           WHERE d.refclassid = 'pg_catalog.pg_operator'::pg_catalog.regclass AND op.oid = d.refobjid
          UNION ALL
          SELECT CASE WHEN c.oid IS NOT NULL THEN 'table_constraint' WHEN ty.oid IS NOT NULL THEN 'domain_constraint' ELSE 'unknown' END::text,
                 COALESCE(cn.nspname, tyn.nspname)::text, COALESCE(c.relname, ty.typname)::text, NULL::text,
                 in_ns.nspname::text, ic.relname::text, COALESCE(c.relnamespace, ty.typnamespace)
            FROM pg_catalog.pg_constraint con
            LEFT JOIN pg_catalog.pg_class c ON con.conrelid <> 0 AND c.oid = con.conrelid
            LEFT JOIN pg_catalog.pg_namespace cn ON cn.oid = c.relnamespace
            LEFT JOIN pg_catalog.pg_class ic ON con.conindid <> 0 AND ic.oid = con.conindid
            LEFT JOIN pg_catalog.pg_namespace in_ns ON in_ns.oid = ic.relnamespace
            LEFT JOIN pg_catalog.pg_type ty ON con.contypid <> 0 AND ty.oid = con.contypid
            LEFT JOIN pg_catalog.pg_namespace tyn ON tyn.oid = ty.typnamespace
           WHERE d.refclassid = 'pg_catalog.pg_constraint'::pg_catalog.regclass AND con.oid = d.refobjid
          UNION ALL
          SELECT 'opclass'::text, n.nspname::text, oc.opcname::text, NULL::text, NULL::text, NULL::text, oc.opcnamespace
            FROM pg_catalog.pg_opclass oc JOIN pg_catalog.pg_namespace n ON n.oid = oc.opcnamespace
           WHERE d.refclassid = 'pg_catalog.pg_opclass'::pg_catalog.regclass AND oc.oid = d.refobjid
          UNION ALL
          SELECT 'collation'::text, n.nspname::text, co.collname::text, NULL::text, NULL::text, NULL::text, co.collnamespace
            FROM pg_catalog.pg_collation co JOIN pg_catalog.pg_namespace n ON n.oid = co.collnamespace
           WHERE d.refclassid = 'pg_catalog.pg_collation'::pg_catalog.regclass AND co.oid = d.refobjid
          UNION ALL
          SELECT 'am'::text, NULL::text, NULL::text, NULL::text, NULL::text, NULL::text, 0::pg_catalog.oid
           WHERE d.refclassid = 'pg_catalog.pg_am'::pg_catalog.regclass
          UNION ALL
          SELECT 'cast'::text, NULL::text, NULL::text, NULL::text, NULL::text, NULL::text, 0::pg_catalog.oid
           WHERE d.refclassid = 'pg_catalog.pg_cast'::pg_catalog.regclass
      ) t ON true
)
SELECT idx, root_oid, parsed, has_edge, ref_oid, is_self_structural,
       target_kind, target_schema, target_name, target_sub, extra_schema, extra_name,
       COALESCE(target_nsp <> 0::pg_catalog.oid AND (
           target_nsp = pg_catalog.pg_my_temp_schema()
           OR pg_catalog.pg_is_other_temp_schema(target_nsp)
       ), false) AS target_is_temp
  FROM mapped
"#;

const ANCESTORS_SQL: &str = r#"
SELECT cn.nspname::text AS desc_schema, c.relname::text AS desc_name,
       'relation'::text AS anc_kind, pn.nspname::text AS anc_schema, p.relname::text AS anc_name
  FROM pg_catalog.pg_inherits h
  JOIN pg_catalog.pg_class c ON c.oid = h.inhrelid
  JOIN pg_catalog.pg_namespace cn ON cn.oid = c.relnamespace
  JOIN pg_catalog.pg_class p ON p.oid = h.inhparent
  JOIN pg_catalog.pg_namespace pn ON pn.oid = p.relnamespace
 WHERE cn.nspname NOT IN ('pg_catalog', 'information_schema') AND pg_catalog.left(cn.nspname, 3) <> 'pg_'
UNION ALL
SELECT cn.nspname::text, c.relname::text, 'type'::text, tn.nspname::text, t.typname::text
  FROM pg_catalog.pg_class c
  JOIN pg_catalog.pg_namespace cn ON cn.oid = c.relnamespace
  JOIN pg_catalog.pg_type t ON t.oid = c.reloftype
  JOIN pg_catalog.pg_namespace tn ON tn.oid = t.typnamespace
 WHERE c.reloftype <> 0
   AND cn.nspname NOT IN ('pg_catalog', 'information_schema') AND pg_catalog.left(cn.nspname, 3) <> 'pg_'
"#;

const CATALOG_NAMES_SQL: &str = r#"
WITH req AS (
    SELECT name FROM pg_catalog.jsonb_to_recordset($1::pg_catalog.text::pg_catalog.jsonb) AS r(name text)
)
SELECT req.name,
       (EXISTS (SELECT 1 FROM pg_catalog.pg_proc p WHERE p.pronamespace = 'pg_catalog'::pg_catalog.regnamespace AND p.proname = req.name)
     OR EXISTS (SELECT 1 FROM pg_catalog.pg_operator o WHERE o.oprnamespace = 'pg_catalog'::pg_catalog.regnamespace AND o.oprname = req.name)
     OR EXISTS (SELECT 1 FROM pg_catalog.pg_type t WHERE t.typnamespace = 'pg_catalog'::pg_catalog.regnamespace AND t.typname = req.name)) AS held
  FROM req
"#;

const COMPANION_TYPES_SQL: &str = r#"
WITH req AS (
    SELECT idx, schema_name, type_name
      FROM pg_catalog.jsonb_to_recordset($1::pg_catalog.text::pg_catalog.jsonb)
        AS r(idx int4, schema_name text, type_name text)
)
SELECT req.idx,
       (n.oid IS NOT NULL
        AND EXISTS (SELECT 1 FROM pg_catalog.pg_type t
                     WHERE t.typnamespace = n.oid AND t.typname = req.type_name)) AS held
  FROM req
  LEFT JOIN pg_catalog.pg_namespace n ON n.nspname = req.schema_name
"#;

/// Read all dependency facts requested by `needed` in one `REPEATABLE READ
/// READ ONLY` snapshot.
pub async fn read(conn: &mut Conn, needed: &FactsNeeded) -> Result<DependencyFacts, DbError> {
    conn.begin(&crate::settled_read::OWN_TRANSACTION).await?;
    let outcome = read_in(conn, needed).await;
    let ended = conn.rollback(&crate::settled_read::OWN_TRANSACTION).await;
    let facts = outcome?;
    ended?;
    Ok(facts)
}

async fn read_in(conn: &mut Conn, needed: &FactsNeeded) -> Result<DependencyFacts, DbError> {
    conn.query(CANONICAL_PATH).await?;
    Ok(DependencyFacts {
        surfaces: read_surfaces(conn, &needed.surfaces).await?,
        relation_ancestors: ReadFact::Known(read_ancestors(conn).await?),
        catalog_names: read_catalog_names(conn, &needed.arriving_names).await?,
        companion_types: read_companion_types(conn, &needed.arriving_companion_types).await?,
    })
}

async fn query_json<T: Serialize>(
    conn: &mut Conn,
    sql: &str,
    reqs: &[T],
) -> Result<Vec<Row>, DbError> {
    let payload = serde_json::to_string(reqs)
        .map_err(|e| DbError::BadRow(format!("serializing dependency-facts request: {e}")))?;
    conn.query_with(sql, &[Param::Str(&payload)]).await
}

fn req_col<'a, T: FromColumn<'a>>(row: &'a Row, col: &str) -> Result<T, DbError> {
    row.try_get::<T>(col)?
        .ok_or_else(|| DbError::BadRow(format!("dependency facts column `{col}` is NULL")))
}

fn opt_str(row: &Row, col: &str) -> Result<Option<String>, DbError> {
    Ok(row.try_get::<&str>(col)?.map(str::to_owned))
}

#[derive(Serialize)]
struct SurfaceReq<'a> {
    idx: i32,
    kind: &'static str,
    schema_name: &'a str,
    obj_name: &'a str,
    sub_name: String,
}

async fn read_surfaces(
    conn: &mut Conn,
    requested: &BTreeSet<Surface>,
) -> Result<BTreeMap<Surface, SurfaceDependencies>, DbError> {
    let list: Vec<Surface> = requested.iter().cloned().collect();
    let mut reqs = Vec::with_capacity(list.len());
    for (i, surface) in list.iter().enumerate() {
        let idx = i32::try_from(i).map_err(|_| DbError::BadRow("surface index overflow".into()))?;
        let req = match surface {
            Surface::Module(ModuleId::Named(o)) => {
                Some(("view", o.schema.as_str(), o.name.as_str(), String::new()))
            }
            Surface::Module(ModuleId::Routine(r)) => Some((
                "routine",
                r.name.schema.as_str(),
                r.name.name.as_str(),
                r.args
                    .iter()
                    .map(pbps_model::RoutineArg::raw)
                    .collect::<Vec<_>>()
                    .join(","),
            )),
            Surface::Module(ModuleId::Trigger { on, name }) => Some((
                "trigger",
                on.schema.as_str(),
                on.name.as_str(),
                name.clone(),
            )),
            Surface::Default(c) => Some((
                "default",
                c.table.schema.as_str(),
                c.table.name.as_str(),
                c.name.raw().to_owned(),
            )),
            Surface::Check { table, name } => Some((
                "check",
                table.schema.as_str(),
                table.name.as_str(),
                name.raw().to_owned(),
            )),
            Surface::Index { table, name } => Some((
                "index",
                table.schema.as_str(),
                table.name.as_str(),
                name.raw().to_owned(),
            )),
            Surface::Namespace(_) | Surface::Table(_) | Surface::Column(_) | Surface::Type(_) => {
                None
            }
        };
        if let Some((kind, schema_name, obj_name, sub_name)) = req {
            reqs.push(SurfaceReq {
                idx,
                kind,
                schema_name,
                obj_name,
                sub_name,
            });
        }
    }
    if reqs.is_empty() {
        return Ok(assemble_surfaces(&list, &[]));
    }
    let rows = query_json(conn, SURFACES_SQL, &reqs).await?;
    let mut raw = Vec::with_capacity(rows.len());
    for row in &rows {
        raw.push(RawSurfaceRow {
            idx: req_col::<i32>(row, "idx")?,
            root_oid: req_col::<i64>(row, "root_oid")?,
            parsed: req_col::<bool>(row, "parsed")?,
            edge: RawEdgeRow {
                has_edge: row.try_get::<bool>("has_edge")?.unwrap_or(false),
                ref_oid: row.try_get::<i64>("ref_oid")?,
                is_self_structural: row.try_get::<bool>("is_self_structural")?.unwrap_or(false),
                target_kind: opt_str(row, "target_kind")?,
                target_schema: opt_str(row, "target_schema")?,
                target_name: opt_str(row, "target_name")?,
                target_sub: opt_str(row, "target_sub")?,
                extra_schema: opt_str(row, "extra_schema")?,
                extra_name: opt_str(row, "extra_name")?,
                target_is_temp: row.try_get::<bool>("target_is_temp")?.unwrap_or(false),
            },
        });
    }
    Ok(assemble_surfaces(&list, &raw))
}

async fn read_ancestors(
    conn: &mut Conn,
) -> Result<BTreeMap<ObjectName, BTreeSet<Ancestor>>, DbError> {
    let rows = conn.query(ANCESTORS_SQL).await?;
    let mut raw = Vec::with_capacity(rows.len());
    for row in &rows {
        raw.push((
            req_col::<&str>(row, "desc_schema")?.to_owned(),
            req_col::<&str>(row, "desc_name")?.to_owned(),
            req_col::<&str>(row, "anc_kind")?.to_owned(),
            req_col::<&str>(row, "anc_schema")?.to_owned(),
            req_col::<&str>(row, "anc_name")?.to_owned(),
        ));
    }
    assemble_ancestors(&raw)
}

#[derive(Serialize)]
struct NameReq<'a> {
    name: &'a str,
}

async fn read_catalog_names(
    conn: &mut Conn,
    names: &BTreeSet<String>,
) -> Result<BTreeMap<String, CatalogName>, DbError> {
    if names.is_empty() {
        return Ok(BTreeMap::new());
    }
    let reqs: Vec<NameReq<'_>> = names.iter().map(|n| NameReq { name: n }).collect();
    let rows = query_json(conn, CATALOG_NAMES_SQL, &reqs).await?;
    let mut raw = Vec::with_capacity(rows.len());
    for row in &rows {
        raw.push((
            req_col::<&str>(row, "name")?.to_owned(),
            row.try_get::<bool>("held")?,
        ));
    }
    Ok(assemble_catalog_names(names, &raw))
}

#[derive(Serialize)]
struct CompanionTypeReq<'a> {
    idx: i32,
    schema_name: &'a str,
    type_name: &'a str,
}

async fn read_companion_types(
    conn: &mut Conn,
    types: &BTreeSet<ObjectName>,
) -> Result<BTreeMap<ObjectName, NameAvailability>, DbError> {
    if types.is_empty() {
        return Ok(BTreeMap::new());
    }
    let list: Vec<ObjectName> = types.iter().cloned().collect();
    let mut reqs = Vec::with_capacity(list.len());
    for (i, obj) in list.iter().enumerate() {
        let idx = i32::try_from(i)
            .map_err(|_| DbError::BadRow("companion type index overflow".into()))?;
        reqs.push(CompanionTypeReq {
            idx,
            schema_name: &obj.schema,
            type_name: &obj.name,
        });
    }
    let rows = query_json(conn, COMPANION_TYPES_SQL, &reqs).await?;
    let mut raw = Vec::with_capacity(rows.len());
    for row in &rows {
        raw.push((req_col::<i32>(row, "idx")?, row.try_get::<bool>("held")?));
    }
    Ok(assemble_companion_types(&list, &raw))
}

// --- Pure mapping and assembly (unit-tested without an engine) ---

#[derive(Debug, Clone, PartialEq, Eq)]
struct RawEdgeRow {
    has_edge: bool,
    ref_oid: Option<i64>,
    is_self_structural: bool,
    target_kind: Option<String>,
    target_schema: Option<String>,
    target_name: Option<String>,
    target_sub: Option<String>,
    extra_schema: Option<String>,
    extra_name: Option<String>,
    target_is_temp: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RawSurfaceRow {
    idx: i32,
    root_oid: i64,
    parsed: bool,
    edge: RawEdgeRow,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum EdgeOutcome {
    Ignore,
    Edge(DependencyEdge),
    Edges(DependencyEdge, DependencyEdge),
    Unreadable,
}

fn classify_schema_target(schema: &str) -> Option<bool> {
    if schema == "pg_catalog" {
        // Safe to drop only because arriving names are checked against pg_catalog separately.
        Some(true)
    } else if schema.starts_with("pg_") {
        None
    } else {
        Some(false)
    }
}

fn classify_named_edge(
    schema: Option<&str>,
    name: Option<&str>,
    make: impl FnOnce(ObjectName) -> DependencyEdge,
) -> EdgeOutcome {
    let (Some(s), Some(n)) = (schema, name) else {
        return EdgeOutcome::Unreadable;
    };
    match classify_schema_target(s) {
        Some(true) => EdgeOutcome::Ignore,
        Some(false) => EdgeOutcome::Edge(make(ObjectName::new(s, n))),
        None => EdgeOutcome::Unreadable,
    }
}

fn classify_edge(row: &RawEdgeRow) -> EdgeOutcome {
    if !row.has_edge || row.is_self_structural {
        return EdgeOutcome::Ignore;
    }
    if row.target_is_temp {
        return EdgeOutcome::Unreadable;
    }
    let (s, n) = (row.target_schema.as_deref(), row.target_name.as_deref());
    match row.target_kind.as_deref() {
        Some("relation" | "row_type") => classify_named_edge(s, n, DependencyEdge::Relation),
        Some("composite_rel" | "type" | "domain_constraint") => {
            classify_named_edge(s, n, DependencyEdge::Type)
        }
        Some("routine" | "operator") => classify_named_edge(s, n, DependencyEdge::Routine),
        Some("column") => {
            let (Some(s), Some(n), Some(col)) = (s, n, row.target_sub.as_deref()) else {
                return EdgeOutcome::Unreadable;
            };
            match classify_schema_target(s) {
                Some(true) => EdgeOutcome::Ignore,
                Some(false) => {
                    EdgeOutcome::Edge(DependencyEdge::Column(TableName::new(s, n).column(col)))
                }
                None => EdgeOutcome::Unreadable,
            }
        }
        Some("table_constraint") => {
            let EdgeOutcome::Edge(table_edge) = classify_named_edge(s, n, DependencyEdge::Relation)
            else {
                return classify_named_edge(s, n, DependencyEdge::Relation);
            };
            match (row.extra_schema.as_deref(), row.extra_name.as_deref()) {
                (Some(ix_s), Some(ix_n)) => match classify_schema_target(ix_s) {
                    Some(false) => EdgeOutcome::Edges(
                        table_edge,
                        DependencyEdge::Relation(ObjectName::new(ix_s, ix_n)),
                    ),
                    Some(true) => EdgeOutcome::Edge(table_edge),
                    None => EdgeOutcome::Unreadable,
                },
                _ => EdgeOutcome::Edge(table_edge),
            }
        }
        Some("opclass" | "collation") if s == Some("pg_catalog") => EdgeOutcome::Ignore,
        Some("am" | "cast") if row.ref_oid.is_some_and(|oid| (1..16_384).contains(&oid)) => {
            EdgeOutcome::Ignore
        }
        Some(_) | None => EdgeOutcome::Unreadable,
    }
}

enum AccState {
    Collected {
        root_oid: i64,
        parsed: bool,
        edges: BTreeSet<DependencyEdge>,
    },
    Unreadable,
}

fn assemble_surfaces(
    requested: &[Surface],
    rows: &[RawSurfaceRow],
) -> BTreeMap<Surface, SurfaceDependencies> {
    let mut acc: BTreeMap<usize, AccState> = BTreeMap::new();
    for row in rows {
        let Ok(idx) = usize::try_from(row.idx) else {
            continue;
        };
        if idx >= requested.len() {
            continue;
        }
        let entry = acc.entry(idx).or_insert_with(|| AccState::Collected {
            root_oid: row.root_oid,
            parsed: row.parsed,
            edges: BTreeSet::new(),
        });
        if let AccState::Collected {
            root_oid,
            parsed,
            edges,
        } = entry
        {
            if *root_oid != row.root_oid || *parsed != row.parsed {
                *entry = AccState::Unreadable;
                continue;
            }
            match classify_edge(&row.edge) {
                EdgeOutcome::Ignore => {}
                EdgeOutcome::Edge(e) => {
                    edges.insert(e);
                }
                EdgeOutcome::Edges(a, b) => {
                    edges.insert(a);
                    edges.insert(b);
                }
                EdgeOutcome::Unreadable => *entry = AccState::Unreadable,
            }
        }
    }
    requested
        .iter()
        .enumerate()
        .map(|(idx, surface)| {
            let state = match acc.remove(&idx) {
                Some(AccState::Collected {
                    parsed: true,
                    edges,
                    ..
                }) => SurfaceDependencies::Parsed(edges),
                Some(AccState::Collected {
                    parsed: false,
                    edges,
                    ..
                }) => SurfaceDependencies::Unparsed(edges),
                Some(AccState::Unreadable) | None => SurfaceDependencies::Unreadable,
            };
            (surface.clone(), state)
        })
        .collect()
}

fn assemble_ancestors(
    rows: &[(String, String, String, String, String)],
) -> Result<BTreeMap<ObjectName, BTreeSet<Ancestor>>, DbError> {
    let mut out: BTreeMap<ObjectName, BTreeSet<Ancestor>> = BTreeMap::new();
    for (desc_schema, desc_name, anc_kind, anc_schema, anc_name) in rows {
        let anc = match anc_kind.as_str() {
            "relation" => Ancestor::Relation(ObjectName::new(anc_schema, anc_name)),
            "type" => Ancestor::Type(ObjectName::new(anc_schema, anc_name)),
            other => {
                return Err(DbError::BadRow(format!(
                    "unexpected ancestor kind `{other}`"
                )));
            }
        };
        out.entry(ObjectName::new(desc_schema, desc_name))
            .or_default()
            .insert(anc);
    }
    Ok(out)
}

fn assemble_catalog_names(
    requested: &BTreeSet<String>,
    rows: &[(String, Option<bool>)],
) -> BTreeMap<String, CatalogName> {
    let seen: BTreeMap<&str, Option<bool>> = rows.iter().map(|(n, h)| (n.as_str(), *h)).collect();
    requested
        .iter()
        .map(|name| {
            let status = match seen.get(name.as_str()).copied().flatten() {
                Some(false) => CatalogName::Clear,
                Some(true) => CatalogName::Held,
                None => CatalogName::Unreadable,
            };
            (name.clone(), status)
        })
        .collect()
}

fn assemble_companion_types(
    requested: &[ObjectName],
    rows: &[(i32, Option<bool>)],
) -> BTreeMap<ObjectName, NameAvailability> {
    let seen: BTreeMap<usize, Option<bool>> = rows
        .iter()
        .filter_map(|&(idx, h)| Some((usize::try_from(idx).ok()?, h)))
        .collect();
    requested
        .iter()
        .enumerate()
        .map(|(i, obj)| {
            let avail = match seen.get(&i).copied().flatten() {
                Some(false) => NameAvailability::Free,
                Some(true) => NameAvailability::Held,
                None => NameAvailability::Unreadable,
            };
            (obj.clone(), avail)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn empty_edge() -> RawEdgeRow {
        RawEdgeRow {
            has_edge: false,
            ref_oid: None,
            is_self_structural: false,
            target_kind: None,
            target_schema: None,
            target_name: None,
            target_sub: None,
            extra_schema: None,
            extra_name: None,
            target_is_temp: false,
        }
    }

    fn edge_row(kind: &str, schema: &str, name: &str) -> RawEdgeRow {
        RawEdgeRow {
            has_edge: true,
            ref_oid: Some(20_000),
            target_kind: Some(kind.into()),
            target_schema: Some(schema.into()),
            target_name: Some(name.into()),
            ..empty_edge()
        }
    }

    #[test]
    fn classify_edge_maps_every_supported_kind_and_drops_pg_catalog_only() {
        assert_eq!(classify_edge(&empty_edge()), EdgeOutcome::Ignore);
        assert_eq!(
            classify_edge(&RawEdgeRow {
                is_self_structural: true,
                ..edge_row("relation", "app", "v")
            }),
            EdgeOutcome::Ignore
        );
        assert_eq!(
            classify_edge(&edge_row("relation", "app", "t")),
            EdgeOutcome::Edge(DependencyEdge::Relation("app.t".parse().unwrap()))
        );
        assert_eq!(
            classify_edge(&edge_row("row_type", "app", "t")),
            EdgeOutcome::Edge(DependencyEdge::Relation("app.t".parse().unwrap()))
        );
        assert_eq!(
            classify_edge(&edge_row("composite_rel", "app", "comp")),
            EdgeOutcome::Edge(DependencyEdge::Type("app.comp".parse().unwrap()))
        );
        assert_eq!(
            classify_edge(&edge_row("type", "app", "dom")),
            EdgeOutcome::Edge(DependencyEdge::Type("app.dom".parse().unwrap()))
        );
        assert_eq!(
            classify_edge(&RawEdgeRow {
                target_sub: Some("note".into()),
                ..edge_row("column", "app", "t")
            }),
            EdgeOutcome::Edge(DependencyEdge::Column(
                "app.t".parse::<TableName>().unwrap().column("note")
            ))
        );
        assert_eq!(
            classify_edge(&edge_row("routine", "app", "f")),
            EdgeOutcome::Edge(DependencyEdge::Routine("app.f".parse().unwrap()))
        );
        assert_eq!(
            classify_edge(&edge_row("operator", "ext", "->")),
            EdgeOutcome::Edge(DependencyEdge::Routine("ext.->".parse().unwrap()))
        );
        assert_eq!(
            classify_edge(&RawEdgeRow {
                extra_schema: Some("app".into()),
                extra_name: Some("t_pkey".into()),
                ..edge_row("table_constraint", "app", "t")
            }),
            EdgeOutcome::Edges(
                DependencyEdge::Relation("app.t".parse().unwrap()),
                DependencyEdge::Relation("app.t_pkey".parse().unwrap()),
            )
        );
        assert_eq!(
            classify_edge(&edge_row("domain_constraint", "app", "dom")),
            EdgeOutcome::Edge(DependencyEdge::Type("app.dom".parse().unwrap()))
        );
        for kind in [
            "relation",
            "row_type",
            "type",
            "routine",
            "operator",
            "opclass",
            "collation",
        ] {
            assert_eq!(
                classify_edge(&edge_row(kind, "pg_catalog", "int4")),
                EdgeOutcome::Ignore,
                "{kind}"
            );
        }
        assert_eq!(
            classify_edge(&RawEdgeRow {
                has_edge: true,
                ref_oid: Some(403),
                target_kind: Some("am".into()),
                ..empty_edge()
            }),
            EdgeOutcome::Ignore
        );
    }

    #[test]
    fn classify_edge_marks_temp_unmappable_non_builtin_and_missing_fields_unreadable() {
        assert_eq!(
            classify_edge(&RawEdgeRow {
                target_is_temp: true,
                ..edge_row("relation", "pg_temp_3", "tmp")
            }),
            EdgeOutcome::Unreadable
        );
        assert_eq!(
            classify_edge(&edge_row("relation", "pg_toast", "pg_toast_1")),
            EdgeOutcome::Unreadable
        );
        for kind in ["opclass", "collation"] {
            assert_eq!(
                classify_edge(&edge_row(kind, "ext", "custom")),
                EdgeOutcome::Unreadable,
                "{kind}"
            );
        }
        for kind in ["am", "cast"] {
            assert_eq!(
                classify_edge(&RawEdgeRow {
                    has_edge: true,
                    ref_oid: Some(20_000),
                    target_kind: Some(kind.into()),
                    ..empty_edge()
                }),
                EdgeOutcome::Unreadable,
                "{kind}"
            );
        }
        assert_eq!(
            classify_edge(&edge_row("unknown", "app", "x")),
            EdgeOutcome::Unreadable
        );
        assert_eq!(
            classify_edge(&edge_row("column", "app", "t")),
            EdgeOutcome::Unreadable
        );
    }

    #[test]
    fn assemble_surfaces_distinguishes_parsed_empty_unparsed_missing_ambiguous_and_unreadable() {
        let v_ok = Surface::Module("app.v_ok".parse().unwrap());
        let v_empty = Surface::Module("app.v_empty".parse().unwrap());
        let f_pl = Surface::Module("app.f_pl()".parse().unwrap());
        let v_missing = Surface::Module("app.v_missing".parse().unwrap());
        let v_ambig = Surface::Module("app.v_ambig".parse().unwrap());
        let v_bad_edge = Surface::Module("app.v_bad_edge".parse().unwrap());
        let requested = vec![
            v_ok.clone(),
            v_empty.clone(),
            f_pl.clone(),
            v_missing.clone(),
            v_ambig.clone(),
            v_bad_edge.clone(),
        ];
        let rows = vec![
            RawSurfaceRow {
                idx: 0,
                root_oid: 100,
                parsed: true,
                edge: edge_row("relation", "app", "t"),
            },
            RawSurfaceRow {
                idx: 1,
                root_oid: 101,
                parsed: true,
                edge: empty_edge(),
            },
            RawSurfaceRow {
                idx: 2,
                root_oid: 102,
                parsed: false,
                edge: edge_row("routine", "app", "g"),
            },
            RawSurfaceRow {
                idx: 4,
                root_oid: 104,
                parsed: true,
                edge: empty_edge(),
            },
            RawSurfaceRow {
                idx: 4,
                root_oid: 105,
                parsed: true,
                edge: empty_edge(),
            },
            RawSurfaceRow {
                idx: 5,
                root_oid: 106,
                parsed: true,
                edge: edge_row("relation", "app", "t"),
            },
            RawSurfaceRow {
                idx: 5,
                root_oid: 106,
                parsed: true,
                edge: edge_row("unknown", "app", "mystery"),
            },
        ];
        let out = assemble_surfaces(&requested, &rows);
        assert_eq!(
            out[&v_ok],
            SurfaceDependencies::Parsed(BTreeSet::from([DependencyEdge::Relation(
                "app.t".parse().unwrap()
            )]))
        );
        assert_eq!(out[&v_empty], SurfaceDependencies::Parsed(BTreeSet::new()));
        assert_eq!(
            out[&f_pl],
            SurfaceDependencies::Unparsed(BTreeSet::from([DependencyEdge::Routine(
                "app.g".parse().unwrap()
            )]))
        );
        assert_eq!(out[&v_missing], SurfaceDependencies::Unreadable);
        assert_eq!(out[&v_ambig], SurfaceDependencies::Unreadable);
        assert_eq!(out[&v_bad_edge], SurfaceDependencies::Unreadable);
    }

    #[test]
    fn assemble_catalog_and_companion_types_fail_closed_on_missing_or_null() {
        let names = BTreeSet::from([
            "clear".into(),
            "held".into(),
            "missing".into(),
            "null".into(),
        ]);
        let cat = assemble_catalog_names(
            &names,
            &[
                ("clear".into(), Some(false)),
                ("held".into(), Some(true)),
                ("null".into(), None),
            ],
        );
        assert_eq!(cat["clear"], CatalogName::Clear);
        assert_eq!(cat["held"], CatalogName::Held);
        assert_eq!(cat["missing"], CatalogName::Unreadable);
        assert_eq!(cat["null"], CatalogName::Unreadable);

        let c0: ObjectName = "early._t".parse().unwrap();
        let c1: ObjectName = "early._u".parse().unwrap();
        let c2: ObjectName = "early._v".parse().unwrap();
        let avail = assemble_companion_types(
            &[c0.clone(), c1.clone(), c2.clone()],
            &[(0, Some(false)), (1, Some(true))],
        );
        assert_eq!(avail[&c0], NameAvailability::Free);
        assert_eq!(avail[&c1], NameAvailability::Held);
        assert_eq!(avail[&c2], NameAvailability::Unreadable);
    }

    #[test]
    fn assemble_ancestors_rejects_unknown_kind_and_keeps_relations_and_types() {
        let ok = assemble_ancestors(&[
            (
                "app".into(),
                "child".into(),
                "relation".into(),
                "app".into(),
                "parent".into(),
            ),
            (
                "app".into(),
                "typed_tbl".into(),
                "type".into(),
                "app".into(),
                "comp".into(),
            ),
        ])
        .unwrap();
        assert_eq!(
            ok[&"app.child".parse().unwrap()],
            BTreeSet::from([Ancestor::Relation("app.parent".parse().unwrap())])
        );
        assert_eq!(
            ok[&"app.typed_tbl".parse().unwrap()],
            BTreeSet::from([Ancestor::Type("app.comp".parse().unwrap())])
        );
        assert!(
            assemble_ancestors(&[(
                "app".into(),
                "x".into(),
                "bogus".into(),
                "app".into(),
                "y".into(),
            )])
            .is_err()
        );
    }
}
