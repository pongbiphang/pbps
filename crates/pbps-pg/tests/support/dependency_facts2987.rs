//! The differential test of the dependency facts (#2987, DEC-2987.2): for
//! each case it builds a database, plans a change, reads PostgreSQL's
//! dependency facts, assesses, applies the change, then re-creates every kept
//! surface on the engine and compares what it bound with what it bound before.
//!
//! Property: no surface answered `Unaffected` by dependency may bind
//! differently after the change. Each form measured to rebind (an arriving
//! function over a built-in, a relation arriving on a type's name, a column
//! arriving, an overload, the forms that record no edge, an inherited, a
//! partitioned and a typed table, the array type of an arriving table, an
//! owned sequence carried across schemas, a routine header) stands beside a
//! neighbour proven unaffected.

use std::collections::BTreeMap;

use pbps_db::{Conn, Driver, Param};
use pbps_diff::resolver::{Answer, UnaffectedReason, assess_with_facts, needed_facts};
use pbps_model::resolver::Surface;
use pbps_model::{
    Change, ChangeSet, CheckConstraint, Column, ColumnChange, IdsFile, Index, IndexColumn,
    IndexKey, ManagedType, Module, ModuleChange, ModuleId, ModuleKind, OwnedSequence,
    PlannedChange, Schema, Table, TableChange, TableName, TypeChange, Uid, UidKind, ValueSource,
};
use pbps_pg::Postgres;

const SERVERS: [&str; 2] = ["PBPS_TEST_PG_OLD_DB", "PBPS_TEST_PG_DB"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Expect {
    Resolve,
    UnaffectedByDep,
    UnaffectedUnparsed,
}

struct TrackedSurface {
    base: Surface,
    kept: Surface,
    drop_sql: String,
    recreate_sql: String,
    expect: Expect,
}

impl TrackedSurface {
    fn kept_same(
        surface: Surface,
        drop_sql: impl Into<String>,
        recreate_sql: impl Into<String>,
        expect: Expect,
    ) -> Self {
        Self {
            kept: surface.clone(),
            base: surface,
            drop_sql: drop_sql.into(),
            recreate_sql: recreate_sql.into(),
            expect,
        }
    }
}

struct Case {
    name: &'static str,
    min_version: i64,
    search_path: &'static str,
    setup_sql: Vec<String>,
    base: Schema,
    desired: Schema,
    changes: Vec<Change>,
    change_sql: Vec<String>,
    tracked: Vec<TrackedSurface>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct BindingSnapshot {
    deparsed: String,
    extras: Vec<String>,
    deps: Vec<String>,
}

fn plain_table(cols: &[(&str, &str)]) -> Table {
    let mut table = Table::default();
    for &(name, ty_str) in cols {
        table
            .columns
            .insert(name.into(), Column::new(ty_str.parse().unwrap()));
    }
    table
}

fn view_mod(def: &str) -> Module {
    Module {
        kind: ModuleKind::View,
        description: None,
        definition: def.to_owned(),
        options: Default::default(),
    }
}

fn routine_mod(kind: ModuleKind, def: &str) -> Module {
    Module {
        kind,
        description: None,
        definition: def.to_owned(),
        options: Default::default(),
    }
}

fn trigger_mod(def: &str) -> Module {
    Module {
        kind: ModuleKind::Trigger,
        description: None,
        definition: def.to_owned(),
        options: Default::default(),
    }
}

fn mod_surface(id: &str) -> Surface {
    Surface::Module(id.parse().unwrap())
}

fn resolve_ids(schema: &Schema, prev: &IdsFile, intents: &[pbps_model::Intent]) -> IdsFile {
    pbps_diff::resolve(
        schema,
        prev,
        intents,
        &pbps_diff::Context {
            operator: "diff-2987".into(),
            today: "2026-11-27".into(),
        },
    )
    .unwrap()
    .ids
}

fn surface_label(surface: &Surface) -> String {
    match surface {
        Surface::Module(id) => id.shown().to_string(),
        Surface::Default(col) => format!("default {}", col.shown()),
        Surface::Check { table, name } => format!("check {}.{}", table.shown(), name.shown()),
        Surface::Index { table, name } => format!("index {}.{}", table.shown(), name.shown()),
        Surface::Namespace(ns) => format!("schema {ns}"),
        Surface::Table(t) => t.shown().to_string(),
        Surface::Column(c) => c.shown().to_string(),
        Surface::Type(t) => format!("type {}", t.shown()),
    }
}

async fn capture_binding(conn: &mut Conn, surface: &Surface) -> Result<BindingSnapshot, String> {
    conn.execute("SAVEPOINT pbps_oracle_cap")
        .await
        .map_err(|e| e.to_string())?;
    let outcome = capture_binding_inner(conn, surface).await;
    let _ = conn.execute("ROLLBACK TO SAVEPOINT pbps_oracle_cap").await;
    let _ = conn.execute("RELEASE SAVEPOINT pbps_oracle_cap").await;
    outcome
}

async fn capture_binding_inner(
    conn: &mut Conn,
    surface: &Surface,
) -> Result<BindingSnapshot, String> {
    conn.query("SELECT pg_catalog.set_config('search_path', '', true)")
        .await
        .map_err(|e| e.to_string())?;
    let (deparsed, extras, deps) = match surface {
        Surface::Module(ModuleId::Named(obj)) => {
            let rows = conn
                .query_with(
                    "SELECT c.oid::int8 AS oid, pg_catalog.pg_get_viewdef(c.oid) AS def
                       FROM pg_catalog.pg_class c
                       JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
                      WHERE n.nspname = $1 AND c.relname = $2 AND c.relkind = 'v'",
                    &[Param::Str(&obj.schema), Param::Str(&obj.name)],
                )
                .await
                .map_err(|e| e.to_string())?;
            let row = rows.first().ok_or("view not found")?;
            let oid = row
                .try_get::<i64>("oid")
                .map_err(|e| e.to_string())?
                .ok_or("null view oid")?;
            let def = row
                .try_get::<&str>("def")
                .map_err(|e| e.to_string())?
                .ok_or("null viewdef")?
                .to_owned();
            let col_rows = conn
                .query_with(
                    "SELECT a.attname::text || ':' || a.atttypid::int8::text || ':' ||
                            pg_catalog.format_type(a.atttypid, a.atttypmod) AS sig
                       FROM pg_catalog.pg_attribute a
                      WHERE a.attrelid = $1::int8::pg_catalog.oid
                        AND a.attnum > 0 AND NOT a.attisdropped
                      ORDER BY a.attnum",
                    &[Param::I64(oid)],
                )
                .await
                .map_err(|e| e.to_string())?;
            let cols = collect_col(&col_rows, "sig")?;
            let dep_rows = conn
                .query_with(
                    "SELECT DISTINCT
                            d.refclassid::pg_catalog.regclass::text || ':' ||
                            d.refobjid::int8::text || ':' || d.refobjsubid::text || ':' ||
                            pg_catalog.pg_describe_object(d.refclassid, d.refobjid, d.refobjsubid) AS dep
                       FROM pg_catalog.pg_depend d
                      WHERE d.deptype = 'n'
                        AND (
                              (d.classid = 'pg_catalog.pg_class'::pg_catalog.regclass
                               AND d.objid = $1::int8::pg_catalog.oid)
                           OR (d.classid = 'pg_catalog.pg_rewrite'::pg_catalog.regclass
                               AND d.objid IN (SELECT rw.oid FROM pg_catalog.pg_rewrite rw
                                                WHERE rw.ev_class = $1::int8::pg_catalog.oid
                                                  AND rw.rulename = '_RETURN'))
                        )
                        AND NOT (d.refclassid = 'pg_catalog.pg_class'::pg_catalog.regclass
                                 AND d.refobjid = $1::int8::pg_catalog.oid)
                        AND d.refclassid <> 'pg_catalog.pg_namespace'::pg_catalog.regclass
                      ORDER BY 1",
                    &[Param::I64(oid)],
                )
                .await
                .map_err(|e| e.to_string())?;
            (def, cols, collect_col(&dep_rows, "dep")?)
        }
        Surface::Module(ModuleId::Routine(r)) => {
            let sig = r
                .args
                .iter()
                .map(pbps_model::RoutineArg::raw)
                .collect::<Vec<_>>()
                .join(",");
            let rows = conn
                .query_with(
                    "SELECT p.oid::int8 AS oid,
                            (p.prosqlbody IS NOT NULL) AS parsed,
                            pg_catalog.pg_get_functiondef(p.oid) AS def,
                            pg_catalog.pg_get_function_arguments(p.oid) AS args,
                            pg_catalog.pg_get_function_result(p.oid) AS ret,
                            COALESCE(pg_catalog.pg_get_expr(p.proargdefaults, 0), '') AS defaults,
                            p.prorettype::int8::text AS ret_oid
                       FROM pg_catalog.pg_proc p
                       JOIN pg_catalog.pg_namespace n ON n.oid = p.pronamespace
                      WHERE n.nspname = $1 AND p.proname = $2 AND p.prokind IN ('f', 'p')
                        AND COALESCE((SELECT pg_catalog.string_agg(
                                               pg_catalog.format_type(u.ty, NULL), ',' ORDER BY u.pos)
                                        FROM pg_catalog.unnest(p.proargtypes)
                                               WITH ORDINALITY AS u(ty, pos)), '') = $3",
                    &[
                        Param::Str(&r.name.schema),
                        Param::Str(&r.name.name),
                        Param::Str(&sig),
                    ],
                )
                .await
                .map_err(|e| e.to_string())?;
            let row = rows.first().ok_or("routine not found")?;
            let oid = row
                .try_get::<i64>("oid")
                .map_err(|e| e.to_string())?
                .ok_or("null proc oid")?;
            let parsed = row
                .try_get::<bool>("parsed")
                .map_err(|e| e.to_string())?
                .unwrap_or(false);
            let def = if parsed {
                row.try_get::<&str>("def")
                    .map_err(|e| e.to_string())?
                    .unwrap_or_default()
                    .to_owned()
            } else {
                String::new()
            };
            let extras = vec![
                row.try_get::<&str>("args")
                    .map_err(|e| e.to_string())?
                    .unwrap_or_default()
                    .to_owned(),
                row.try_get::<&str>("ret")
                    .map_err(|e| e.to_string())?
                    .unwrap_or_default()
                    .to_owned(),
                row.try_get::<&str>("defaults")
                    .map_err(|e| e.to_string())?
                    .unwrap_or_default()
                    .to_owned(),
                row.try_get::<&str>("ret_oid")
                    .map_err(|e| e.to_string())?
                    .unwrap_or_default()
                    .to_owned(),
            ];
            let dep_rows = conn
                .query_with(
                    "SELECT DISTINCT
                            d.refclassid::pg_catalog.regclass::text || ':' ||
                            d.refobjid::int8::text || ':' || d.refobjsubid::text || ':' ||
                            pg_catalog.pg_describe_object(d.refclassid, d.refobjid, d.refobjsubid) AS dep
                       FROM pg_catalog.pg_depend d
                      WHERE d.classid = 'pg_catalog.pg_proc'::pg_catalog.regclass
                        AND d.objid = $1::int8::pg_catalog.oid
                        AND d.deptype = 'n'
                        AND d.refclassid NOT IN (
                            'pg_catalog.pg_namespace'::pg_catalog.regclass,
                            'pg_catalog.pg_language'::pg_catalog.regclass
                        )
                      ORDER BY 1",
                    &[Param::I64(oid)],
                )
                .await
                .map_err(|e| e.to_string())?;
            (def, extras, collect_col(&dep_rows, "dep")?)
        }
        Surface::Module(ModuleId::Trigger { on, name }) => {
            let rows = conn
                .query_with(
                    "SELECT tg.oid::int8 AS oid, pg_catalog.pg_get_triggerdef(tg.oid) AS def,
                            tg.tgfoid::int8::text AS fn_oid
                       FROM pg_catalog.pg_trigger tg
                       JOIN pg_catalog.pg_class c ON c.oid = tg.tgrelid
                       JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
                      WHERE n.nspname = $1 AND c.relname = $2 AND tg.tgname = $3
                        AND NOT tg.tgisinternal",
                    &[
                        Param::Str(&on.schema),
                        Param::Str(&on.name),
                        Param::Str(name),
                    ],
                )
                .await
                .map_err(|e| e.to_string())?;
            let row = rows.first().ok_or("trigger not found")?;
            let oid = row
                .try_get::<i64>("oid")
                .map_err(|e| e.to_string())?
                .ok_or("null trigger oid")?;
            let def = row
                .try_get::<&str>("def")
                .map_err(|e| e.to_string())?
                .unwrap_or_default()
                .to_owned();
            let fn_oid = row
                .try_get::<&str>("fn_oid")
                .map_err(|e| e.to_string())?
                .unwrap_or_default()
                .to_owned();
            let dep_rows = conn
                .query_with(
                    "SELECT DISTINCT
                            d.refclassid::pg_catalog.regclass::text || ':' ||
                            d.refobjid::int8::text || ':' || d.refobjsubid::text || ':' ||
                            pg_catalog.pg_describe_object(d.refclassid, d.refobjid, d.refobjsubid) AS dep
                       FROM pg_catalog.pg_depend d
                      WHERE d.classid = 'pg_catalog.pg_trigger'::pg_catalog.regclass
                        AND d.objid = $1::int8::pg_catalog.oid
                        AND d.deptype IN ('n', 'a')
                      ORDER BY 1",
                    &[Param::I64(oid)],
                )
                .await
                .map_err(|e| e.to_string())?;
            (def, vec![fn_oid], collect_col(&dep_rows, "dep")?)
        }
        Surface::Default(col) => {
            let rows = conn
                .query_with(
                    "SELECT ad.oid::int8 AS oid, ad.adrelid::int8 AS rel_oid, ad.adnum::int4 AS attnum,
                            pg_catalog.pg_get_expr(ad.adbin, ad.adrelid) AS def
                       FROM pg_catalog.pg_attrdef ad
                       JOIN pg_catalog.pg_attribute a ON a.attrelid = ad.adrelid AND a.attnum = ad.adnum
                       JOIN pg_catalog.pg_class c ON c.oid = ad.adrelid
                       JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
                      WHERE n.nspname = $1 AND c.relname = $2 AND a.attname = $3
                        AND a.attnum > 0 AND NOT a.attisdropped",
                    &[
                        Param::Str(&col.table.schema),
                        Param::Str(&col.table.name),
                        Param::Str(col.name.raw()),
                    ],
                )
                .await
                .map_err(|e| e.to_string())?;
            let row = rows.first().ok_or("default not found")?;
            let oid = row
                .try_get::<i64>("oid")
                .map_err(|e| e.to_string())?
                .ok_or("null attrdef oid")?;
            let rel_oid = row
                .try_get::<i64>("rel_oid")
                .map_err(|e| e.to_string())?
                .ok_or("null adrelid")?;
            let attnum = row
                .try_get::<i32>("attnum")
                .map_err(|e| e.to_string())?
                .ok_or("null adnum")?;
            let def = row
                .try_get::<&str>("def")
                .map_err(|e| e.to_string())?
                .unwrap_or_default()
                .to_owned();
            let dep_rows = conn
                .query_with(
                    "SELECT DISTINCT
                            d.refclassid::pg_catalog.regclass::text || ':' ||
                            d.refobjid::int8::text || ':' || d.refobjsubid::text || ':' ||
                            pg_catalog.pg_describe_object(d.refclassid, d.refobjid, d.refobjsubid) AS dep
                       FROM pg_catalog.pg_depend d
                      WHERE d.classid = 'pg_catalog.pg_attrdef'::pg_catalog.regclass
                        AND d.objid = $1::int8::pg_catalog.oid
                        AND d.deptype IN ('n', 'a')
                        AND NOT (d.refclassid = 'pg_catalog.pg_class'::pg_catalog.regclass
                                 AND d.refobjid = $2::int8::pg_catalog.oid
                                 AND d.refobjsubid = $3::int4)
                      ORDER BY 1",
                    &[Param::I64(oid), Param::I64(rel_oid), Param::I32(attnum)],
                )
                .await
                .map_err(|e| e.to_string())?;
            (def, Vec::new(), collect_col(&dep_rows, "dep")?)
        }
        Surface::Check { table, name } => {
            let rows = conn
                .query_with(
                    "SELECT con.oid::int8 AS oid,
                            pg_catalog.pg_get_expr(con.conbin, con.conrelid) AS def
                       FROM pg_catalog.pg_constraint con
                       JOIN pg_catalog.pg_class c ON c.oid = con.conrelid
                       JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
                      WHERE n.nspname = $1 AND c.relname = $2 AND con.conname = $3
                        AND con.contype = 'c'",
                    &[
                        Param::Str(&table.schema),
                        Param::Str(&table.name),
                        Param::Str(name.raw()),
                    ],
                )
                .await
                .map_err(|e| e.to_string())?;
            let row = rows.first().ok_or("check not found")?;
            let oid = row
                .try_get::<i64>("oid")
                .map_err(|e| e.to_string())?
                .ok_or("null check oid")?;
            let def = row
                .try_get::<&str>("def")
                .map_err(|e| e.to_string())?
                .unwrap_or_default()
                .to_owned();
            let dep_rows = conn
                .query_with(
                    "SELECT DISTINCT
                            d.refclassid::pg_catalog.regclass::text || ':' ||
                            d.refobjid::int8::text || ':' || d.refobjsubid::text || ':' ||
                            pg_catalog.pg_describe_object(d.refclassid, d.refobjid, d.refobjsubid) AS dep
                       FROM pg_catalog.pg_depend d
                      WHERE d.classid = 'pg_catalog.pg_constraint'::pg_catalog.regclass
                        AND d.objid = $1::int8::pg_catalog.oid
                        AND d.deptype IN ('n', 'a')
                      ORDER BY 1",
                    &[Param::I64(oid)],
                )
                .await
                .map_err(|e| e.to_string())?;
            (def, Vec::new(), collect_col(&dep_rows, "dep")?)
        }
        Surface::Index { table, name } => {
            let rows = conn
                .query_with(
                    "SELECT idx.oid::int8 AS oid, pg_catalog.pg_get_indexdef(idx.oid) AS def
                       FROM pg_catalog.pg_index i
                       JOIN pg_catalog.pg_class tbl ON tbl.oid = i.indrelid
                       JOIN pg_catalog.pg_namespace n ON n.oid = tbl.relnamespace
                       JOIN pg_catalog.pg_class idx ON idx.oid = i.indexrelid
                      WHERE n.nspname = $1 AND tbl.relname = $2 AND idx.relname = $3",
                    &[
                        Param::Str(&table.schema),
                        Param::Str(&table.name),
                        Param::Str(name.raw()),
                    ],
                )
                .await
                .map_err(|e| e.to_string())?;
            let row = rows.first().ok_or("index not found")?;
            let oid = row
                .try_get::<i64>("oid")
                .map_err(|e| e.to_string())?
                .ok_or("null index oid")?;
            let def = row
                .try_get::<&str>("def")
                .map_err(|e| e.to_string())?
                .unwrap_or_default()
                .to_owned();
            let dep_rows = conn
                .query_with(
                    "SELECT DISTINCT
                            d.refclassid::pg_catalog.regclass::text || ':' ||
                            d.refobjid::int8::text || ':' || d.refobjsubid::text || ':' ||
                            pg_catalog.pg_describe_object(d.refclassid, d.refobjid, d.refobjsubid) AS dep
                       FROM pg_catalog.pg_depend d
                      WHERE d.classid = 'pg_catalog.pg_class'::pg_catalog.regclass
                        AND d.objid = $1::int8::pg_catalog.oid
                        AND d.deptype IN ('n', 'a')
                        AND d.refclassid <> 'pg_catalog.pg_namespace'::pg_catalog.regclass
                      ORDER BY 1",
                    &[Param::I64(oid)],
                )
                .await
                .map_err(|e| e.to_string())?;
            (def, Vec::new(), collect_col(&dep_rows, "dep")?)
        }
        Surface::Namespace(_) | Surface::Table(_) | Surface::Column(_) | Surface::Type(_) => {
            return Err("unsupported surface kind for oracle".into());
        }
    };
    Ok(BindingSnapshot {
        deparsed,
        extras,
        deps,
    })
}

fn collect_col(rows: &[pbps_db::Row], col: &str) -> Result<Vec<String>, String> {
    rows.iter()
        .map(|r| {
            r.try_get::<&str>(col)
                .map_err(|e| e.to_string())
                .and_then(|v| v.map(str::to_owned).ok_or_else(|| format!("null {col}")))
        })
        .collect()
}

fn corpus() -> Vec<Case> {
    let mut cases = Vec::new();

    // 1. built-in function name `lower` arriving captures view calling `lower(txt)`.
    {
        let mut base = Schema::default();
        base.tables
            .insert("app.t".parse().unwrap(), plain_table(&[("txt", "varchar")]));
        base.modules.insert(
            "app.v_lower".parse().unwrap(),
            view_mod("SELECT lower(txt) AS r FROM app.t"),
        );
        let mut desired = base.clone();
        desired.modules.insert(
            "early.lower(character varying)".parse().unwrap(),
            routine_mod(
                ModuleKind::Function,
                "RETURNS text LANGUAGE sql RETURN 'hi'",
            ),
        );
        cases.push(Case {
            name: "builtin_fn_lower",
            min_version: 0,
            search_path: "early, app, public",
            setup_sql: vec![
                "CREATE SCHEMA early; CREATE SCHEMA app;".into(),
                "CREATE TABLE app.t (txt varchar);".into(),
                "CREATE VIEW app.v_lower AS SELECT lower(txt) AS r FROM app.t;".into(),
            ],
            base,
            desired,
            changes: vec![Change::Module(ModuleChange::CreateModule {
                id: "early.lower(character varying)".parse().unwrap(),
                module: Box::new(routine_mod(
                    ModuleKind::Function,
                    "RETURNS text LANGUAGE sql RETURN 'hi'",
                )),
            })],
            change_sql: vec![
                "CREATE FUNCTION early.lower(varchar) RETURNS text LANGUAGE sql RETURN 'hi';"
                    .into(),
            ],
            tracked: vec![TrackedSurface::kept_same(
                mod_surface("app.v_lower"),
                "DROP VIEW app.v_lower",
                "CREATE VIEW app.v_lower AS SELECT lower(txt) AS r FROM app.t",
                Expect::Resolve,
            )],
        });
    }

    // 2. arriving function on a built-in type name `uuid` captures `uuid(txt)`.
    {
        let mut base = Schema::default();
        base.tables
            .insert("app.t".parse().unwrap(), plain_table(&[("txt", "text")]));
        base.modules.insert(
            "app.v_uuid".parse().unwrap(),
            view_mod("SELECT uuid(txt) AS r FROM app.t"),
        );
        let mut desired = base.clone();
        desired.modules.insert(
            "early.uuid(text)".parse().unwrap(),
            routine_mod(ModuleKind::Function, "RETURNS text LANGUAGE sql RETURN 'x'"),
        );
        cases.push(Case {
            name: "builtin_type_uuid",
            min_version: 0,
            search_path: "early, app, public",
            setup_sql: vec![
                "CREATE SCHEMA early; CREATE SCHEMA app;".into(),
                "CREATE TABLE app.t (txt text);".into(),
                "CREATE VIEW app.v_uuid AS SELECT uuid(txt) AS r FROM app.t;".into(),
            ],
            base,
            desired,
            changes: vec![Change::Module(ModuleChange::CreateModule {
                id: "early.uuid(text)".parse().unwrap(),
                module: Box::new(routine_mod(
                    ModuleKind::Function,
                    "RETURNS text LANGUAGE sql RETURN 'x'",
                )),
            })],
            change_sql: vec![
                "CREATE FUNCTION early.uuid(text) RETURNS text LANGUAGE sql RETURN 'x';".into(),
            ],
            tracked: vec![TrackedSurface::kept_same(
                mod_surface("app.v_uuid"),
                "DROP VIEW app.v_uuid",
                "CREATE VIEW app.v_uuid AS SELECT uuid(txt) AS r FROM app.t",
                Expect::Resolve,
            )],
        });
    }

    // An array type written as a function is a cast until a routine of its
    // name arrives: the recorded dependency is on the array type itself.
    {
        let mut base = Schema::default();
        base.tables
            .insert("app.t".parse().unwrap(), plain_table(&[("txt", "text")]));
        base.modules.insert(
            "app.v_moods".parse().unwrap(),
            view_mod("SELECT _mood(txt) AS r FROM app.t"),
        );
        base.modules.insert(
            "app.v_plain".parse().unwrap(),
            view_mod("SELECT txt FROM app.t"),
        );
        let mut desired = base.clone();
        desired.modules.insert(
            "early._mood(text)".parse().unwrap(),
            routine_mod(ModuleKind::Function, "RETURNS text LANGUAGE sql RETURN 'x'"),
        );
        cases.push(Case {
            name: "array_type_called_as_function",
            min_version: 0,
            search_path: "early, app, public",
            setup_sql: vec![
                "CREATE SCHEMA early; CREATE SCHEMA app;".into(),
                "CREATE TYPE app.mood AS ENUM ('a', 'b'); CREATE TABLE app.t (txt text);".into(),
                "CREATE VIEW app.v_moods AS SELECT _mood(txt) AS r FROM app.t;".into(),
                "CREATE VIEW app.v_plain AS SELECT txt FROM app.t;".into(),
            ],
            base,
            desired,
            changes: vec![Change::Module(ModuleChange::CreateModule {
                id: "early._mood(text)".parse().unwrap(),
                module: Box::new(routine_mod(
                    ModuleKind::Function,
                    "RETURNS text LANGUAGE sql RETURN 'x'",
                )),
            })],
            change_sql: vec![
                "CREATE FUNCTION early._mood(text) RETURNS text LANGUAGE sql RETURN 'x';".into(),
            ],
            tracked: vec![
                TrackedSurface::kept_same(
                    mod_surface("app.v_moods"),
                    "DROP VIEW app.v_moods",
                    "CREATE VIEW app.v_moods AS SELECT _mood(txt) AS r FROM app.t",
                    Expect::Resolve,
                ),
                TrackedSurface::kept_same(
                    mod_surface("app.v_plain"),
                    "DROP VIEW app.v_plain",
                    "CREATE VIEW app.v_plain AS SELECT txt FROM app.t",
                    Expect::UnaffectedByDep,
                ),
            ],
        });
    }

    // 3. built-in aggregate name `max` arriving captures `max(txt)`.
    {
        let mut base = Schema::default();
        base.tables
            .insert("app.t".parse().unwrap(), plain_table(&[("txt", "varchar")]));
        base.modules.insert(
            "app.v_max".parse().unwrap(),
            view_mod("SELECT max(txt) AS r FROM app.t"),
        );
        let mut desired = base.clone();
        desired.modules.insert(
            "early.max(character varying)".parse().unwrap(),
            routine_mod(ModuleKind::Function, "RETURNS text LANGUAGE sql RETURN 'm'"),
        );
        cases.push(Case {
            name: "builtin_agg_max",
            min_version: 0,
            search_path: "early, app, public",
            setup_sql: vec![
                "CREATE SCHEMA early; CREATE SCHEMA app;".into(),
                "CREATE TABLE app.t (txt varchar);".into(),
                "CREATE VIEW app.v_max AS SELECT max(txt) AS r FROM app.t;".into(),
            ],
            base,
            desired,
            changes: vec![Change::Module(ModuleChange::CreateModule {
                id: "early.max(character varying)".parse().unwrap(),
                module: Box::new(routine_mod(
                    ModuleKind::Function,
                    "RETURNS text LANGUAGE sql RETURN 'm'",
                )),
            })],
            change_sql: vec![
                "CREATE FUNCTION early.max_sf(text, varchar) RETURNS text LANGUAGE sql RETURN 'm';"
                    .into(),
                "CREATE AGGREGATE early.max(varchar) (SFUNC = early.max_sf, STYPE = text);".into(),
            ],
            tracked: vec![TrackedSurface::kept_same(
                mod_surface("app.v_max"),
                "DROP VIEW app.v_max",
                "CREATE VIEW app.v_max AS SELECT max(txt) AS r FROM app.t",
                Expect::Resolve,
            )],
        });
    }

    // 4. relation arriving on a type name captures type lookup; neighbour is Unaffected.
    {
        let mut base = Schema::default();
        base.tables.insert(
            "late.other".parse().unwrap(),
            plain_table(&[("id", "integer")]),
        );
        base.modules.insert(
            "app.v_type".parse().unwrap(),
            view_mod("SELECT NULL::item AS r"),
        );
        base.modules.insert(
            "app.v_other".parse().unwrap(),
            view_mod("SELECT id FROM late.other"),
        );
        let mut desired = base.clone();
        let arriving = plain_table(&[("y", "text")]);
        desired
            .tables
            .insert("early.item".parse().unwrap(), arriving.clone());
        cases.push(Case {
            name: "relation_arriving_on_type",
            min_version: 0,
            search_path: "early, late, app, public",
            setup_sql: vec![
                "CREATE SCHEMA early; CREATE SCHEMA late; CREATE SCHEMA app;".into(),
                "CREATE TYPE late.item AS (x int);".into(),
                "CREATE TABLE late.other (id int);".into(),
                "CREATE VIEW app.v_type AS SELECT NULL::item AS r;".into(),
                "CREATE VIEW app.v_other AS SELECT id FROM late.other;".into(),
            ],
            base,
            desired,
            changes: vec![Change::Table(TableChange::CreateTable {
                uid: Uid::generate(UidKind::Table),
                name: "early.item".parse().unwrap(),
                table: Box::new(arriving),
                beside_default: None,
                moves_at_most: None,
            })],
            change_sql: vec!["CREATE TABLE early.item (y text);".into()],
            tracked: vec![
                TrackedSurface::kept_same(
                    mod_surface("app.v_type"),
                    "DROP VIEW app.v_type",
                    "CREATE VIEW app.v_type AS SELECT NULL::item AS r",
                    Expect::Resolve,
                ),
                TrackedSurface::kept_same(
                    mod_surface("app.v_other"),
                    "DROP VIEW app.v_other",
                    "CREATE VIEW app.v_other AS SELECT id FROM late.other",
                    Expect::UnaffectedByDep,
                ),
            ],
        });
    }

    // 5. column arrival on `app.t` captures `SELECT * FROM app.t`; neighbour `app.u` is Unaffected.
    {
        let mut base = Schema::default();
        base.tables
            .insert("app.t".parse().unwrap(), plain_table(&[("id", "integer")]));
        base.tables
            .insert("app.u".parse().unwrap(), plain_table(&[("id", "integer")]));
        base.modules
            .insert("app.v_t".parse().unwrap(), view_mod("SELECT * FROM app.t"));
        base.modules
            .insert("app.v_u".parse().unwrap(), view_mod("SELECT id FROM app.u"));
        let mut desired = base.clone();
        desired
            .tables
            .get_mut(&"app.t".parse().unwrap())
            .unwrap()
            .columns
            .insert("extra".into(), Column::new("integer".parse().unwrap()));
        cases.push(Case {
            name: "column_arrival",
            min_version: 0,
            search_path: "app, public",
            setup_sql: vec![
                "CREATE SCHEMA app;".into(),
                "CREATE TABLE app.t (id int); CREATE TABLE app.u (id int);".into(),
                "CREATE VIEW app.v_t AS SELECT * FROM app.t;".into(),
                "CREATE VIEW app.v_u AS SELECT id FROM app.u;".into(),
            ],
            base,
            desired,
            changes: vec![Change::Column(ColumnChange::AddColumn {
                uid: Uid::generate(UidKind::Column),
                table: "app.t".parse().unwrap(),
                name: "extra".into(),
                column: Box::new(Column::new("integer".parse().unwrap())),
            })],
            change_sql: vec!["ALTER TABLE app.t ADD COLUMN extra int;".into()],
            tracked: vec![
                TrackedSurface::kept_same(
                    mod_surface("app.v_t"),
                    "DROP VIEW app.v_t",
                    "CREATE VIEW app.v_t AS SELECT * FROM app.t",
                    Expect::Resolve,
                ),
                TrackedSurface::kept_same(
                    mod_surface("app.v_u"),
                    "DROP VIEW app.v_u",
                    "CREATE VIEW app.v_u AS SELECT id FROM app.u",
                    Expect::UnaffectedByDep,
                ),
            ],
        });
    }

    // 6. arriving overload captures call; neighbour view is Unaffected.
    {
        let mut base = Schema::default();
        base.tables
            .insert("app.u".parse().unwrap(), plain_table(&[("id", "integer")]));
        base.modules.insert(
            "app.v_call".parse().unwrap(),
            view_mod("SELECT calc(1) AS r"),
        );
        base.modules
            .insert("app.v_u".parse().unwrap(), view_mod("SELECT id FROM app.u"));
        let mut desired = base.clone();
        desired.modules.insert(
            "early.calc(integer)".parse().unwrap(),
            routine_mod(ModuleKind::Function, "RETURNS int LANGUAGE sql RETURN 2"),
        );
        cases.push(Case {
            name: "overload_arrival",
            min_version: 0,
            search_path: "early, late, app, public",
            setup_sql: vec![
                "CREATE SCHEMA early; CREATE SCHEMA late; CREATE SCHEMA app;".into(),
                "CREATE TABLE app.u (id int);".into(),
                "CREATE FUNCTION late.calc(numeric) RETURNS int LANGUAGE sql RETURN 1;".into(),
                "CREATE VIEW app.v_call AS SELECT calc(1) AS r;".into(),
                "CREATE VIEW app.v_u AS SELECT id FROM app.u;".into(),
            ],
            base,
            desired,
            changes: vec![Change::Module(ModuleChange::CreateModule {
                id: "early.calc(integer)".parse().unwrap(),
                module: Box::new(routine_mod(
                    ModuleKind::Function,
                    "RETURNS int LANGUAGE sql RETURN 2",
                )),
            })],
            change_sql: vec![
                "CREATE FUNCTION early.calc(int) RETURNS int LANGUAGE sql RETURN 2;".into(),
            ],
            tracked: vec![
                TrackedSurface::kept_same(
                    mod_surface("app.v_call"),
                    "DROP VIEW app.v_call",
                    "CREATE VIEW app.v_call AS SELECT calc(1) AS r",
                    Expect::Resolve,
                ),
                TrackedSurface::kept_same(
                    mod_surface("app.v_u"),
                    "DROP VIEW app.v_u",
                    "CREATE VIEW app.v_u AS SELECT id FROM app.u",
                    Expect::UnaffectedByDep,
                ),
            ],
        });
    }

    // 7. empty typed array `ARRAY[]::n[]` leaves no edge; text test catches arrival.
    {
        let mut base = Schema::default();
        base.tables.insert(
            "late.other".parse().unwrap(),
            plain_table(&[("id", "integer")]),
        );
        base.modules.insert(
            "late.z_empty".parse().unwrap(),
            view_mod("SELECT 1 AS r WHERE ARRAY[]::n[] IS NULL"),
        );
        base.modules.insert(
            "late.v_other".parse().unwrap(),
            view_mod("SELECT id FROM late.other"),
        );
        let mut desired = base.clone();
        let tbl = plain_table(&[("y", "text")]);
        desired
            .tables
            .insert("early.n".parse().unwrap(), tbl.clone());
        cases.push(Case {
            name: "empty_typed_array",
            min_version: 0,
            search_path: "early, late, public",
            setup_sql: vec![
                "CREATE SCHEMA early; CREATE SCHEMA late;".into(),
                "CREATE TYPE late.n AS (x int);".into(),
                "CREATE TABLE late.other (id int);".into(),
                "CREATE VIEW late.z_empty AS SELECT 1 AS r WHERE ARRAY[]::n[] IS NULL;".into(),
                "CREATE VIEW late.v_other AS SELECT id FROM late.other;".into(),
            ],
            base,
            desired,
            changes: vec![Change::Table(TableChange::CreateTable {
                uid: Uid::generate(UidKind::Table),
                name: "early.n".parse().unwrap(),
                table: Box::new(tbl),
                beside_default: None,
                moves_at_most: None,
            })],
            change_sql: vec!["CREATE TABLE early.n (y text);".into()],
            tracked: vec![
                TrackedSurface::kept_same(
                    mod_surface("late.z_empty"),
                    "DROP VIEW late.z_empty",
                    "CREATE VIEW late.z_empty AS SELECT 1 AS r WHERE ARRAY[]::n[] IS NULL",
                    Expect::Resolve,
                ),
                TrackedSurface::kept_same(
                    mod_surface("late.v_other"),
                    "DROP VIEW late.v_other",
                    "CREATE VIEW late.v_other AS SELECT id FROM late.other",
                    Expect::UnaffectedByDep,
                ),
            ],
        });
    }

    // 8. PostgreSQL 17 and later: `json_value(... RETURNING dom)` in WHERE leaves no edge; text test catches it.
    {
        let mut base = Schema::default();
        base.tables.insert(
            "late.other".parse().unwrap(),
            plain_table(&[("id", "integer")]),
        );
        base.modules.insert(
            "late.v_json".parse().unwrap(),
            view_mod(
                "SELECT 1 AS r WHERE json_value('{\"a\":1}'::jsonb, '$.a' RETURNING dom) IS NOT NULL",
            ),
        );
        base.modules.insert(
            "late.v_other".parse().unwrap(),
            view_mod("SELECT id FROM late.other"),
        );
        let mut desired = base.clone();
        let tbl = plain_table(&[("id", "integer")]);
        desired
            .tables
            .insert("early.dom".parse().unwrap(), tbl.clone());
        cases.push(Case {
            name: "json_value_returning",
            min_version: 170_000,
            search_path: "early, late, public",
            setup_sql: vec![
                "CREATE SCHEMA early; CREATE SCHEMA late;".into(),
                "CREATE DOMAIN late.dom AS integer;".into(),
                "CREATE TABLE late.other (id int);".into(),
                "CREATE VIEW late.v_json AS SELECT 1 AS r WHERE json_value('{\"a\":1}'::jsonb, '$.a' RETURNING dom) IS NOT NULL;".into(),
                "CREATE VIEW late.v_other AS SELECT id FROM late.other;".into(),
            ],
            base,
            desired,
            changes: vec![Change::Table(TableChange::CreateTable {
                uid: Uid::generate(UidKind::Table),
                name: "early.dom".parse().unwrap(),
                table: Box::new(tbl),
                beside_default: None,
                moves_at_most: None,
            })],
            change_sql: vec!["CREATE TABLE early.dom (id int);".into()],
            tracked: vec![
                TrackedSurface::kept_same(
                    mod_surface("late.v_json"),
                    "DROP VIEW late.v_json",
                    "CREATE VIEW late.v_json AS SELECT 1 AS r WHERE json_value('{\"a\":1}'::jsonb, '$.a' RETURNING dom) IS NOT NULL",
                    Expect::Resolve,
                ),
                TrackedSurface::kept_same(
                    mod_surface("late.v_other"),
                    "DROP VIEW late.v_other",
                    "CREATE VIEW late.v_other AS SELECT id FROM late.other",
                    Expect::UnaffectedByDep,
                ),
            ],
        });
    }

    // 9. `BEGIN ATOMIC` procedure with `MERGE` target column; relation granularity catches it.
    {
        let mut base = Schema::default();
        base.tables.insert(
            "app.t".parse().unwrap(),
            plain_table(&[("id", "integer"), ("a", "integer")]),
        );
        base.tables.insert(
            "app.u".parse().unwrap(),
            plain_table(&[("id", "integer"), ("k", "integer")]),
        );
        base.tables.insert(
            "app.other".parse().unwrap(),
            plain_table(&[("id", "integer")]),
        );
        let merge_def = "LANGUAGE sql BEGIN ATOMIC \
            MERGE INTO app.u USING app.t ON app.u.id = app.t.id \
            WHEN MATCHED THEN UPDATE SET k = app.t.a \
            WHEN NOT MATCHED THEN INSERT (id, k) VALUES (app.t.id, app.t.a); END";
        base.modules.insert(
            "app.c_merge()".parse().unwrap(),
            routine_mod(ModuleKind::Procedure, merge_def),
        );
        base.modules.insert(
            "app.v_other".parse().unwrap(),
            view_mod("SELECT id FROM app.other"),
        );
        let mut desired = base.clone();
        desired
            .tables
            .get_mut(&"app.u".parse().unwrap())
            .unwrap()
            .columns
            .get_mut("k")
            .unwrap()
            .ty = "bigint".parse().unwrap();
        cases.push(Case {
            name: "merge_target_column",
            min_version: 0,
            search_path: "app, public",
            setup_sql: vec![
                "CREATE SCHEMA app;".into(),
                "CREATE TABLE app.t (id int, a int); CREATE TABLE app.u (id int, k int); CREATE TABLE app.other (id int);".into(),
                format!("CREATE PROCEDURE app.c_merge() {merge_def};"),
                "CREATE VIEW app.v_other AS SELECT id FROM app.other;".into(),
            ],
            base,
            desired,
            changes: vec![Change::Column(ColumnChange::AlterColumnType {
                uid: Uid::generate(UidKind::Column),
                column: "app.u".parse::<TableName>().unwrap().column("k"),
                from: "integer".parse().unwrap(),
                to: "bigint".parse().unwrap(),
                from_nullable: true,
                to_nullable: true,
                from_collation: None,
                to_collation: None,
                through_external_partitions: false,
                using: None,
            })],
            change_sql: vec!["ALTER TABLE app.u ALTER COLUMN k TYPE bigint;".into()],
            tracked: vec![
                TrackedSurface::kept_same(
                    mod_surface("app.c_merge()"),
                    "DROP PROCEDURE app.c_merge()",
                    format!("CREATE PROCEDURE app.c_merge() {merge_def}"),
                    Expect::Resolve,
                ),
                TrackedSurface::kept_same(
                    mod_surface("app.v_other"),
                    "DROP VIEW app.v_other",
                    "CREATE VIEW app.v_other AS SELECT id FROM app.other",
                    Expect::UnaffectedByDep,
                ),
            ],
        });
    }

    // 10. runtime lookup `'n'::text::regclass`; text test conservatively holds `Resolve`.
    {
        let mut base = Schema::default();
        base.tables.insert(
            "late.other".parse().unwrap(),
            plain_table(&[("id", "integer")]),
        );
        base.modules.insert(
            "late.v_dyn".parse().unwrap(),
            view_mod("SELECT to_regclass('n') AS r, 1 AS x WHERE 'n' <> ''"),
        );
        base.modules.insert(
            "late.v_other".parse().unwrap(),
            view_mod("SELECT id FROM late.other"),
        );
        // Mention `n` outside string literal too (as an identifier in an expression)
        // so the lexical scanner holds it, matching the documented text test.
        base.modules.insert(
            "late.v_dyn_ident".parse().unwrap(),
            view_mod("SELECT to_regclass(n::text) AS r FROM (SELECT 'n'::text AS n) q"),
        );
        let mut desired = base.clone();
        let tbl = plain_table(&[("id", "integer")]);
        desired
            .tables
            .insert("early.n".parse().unwrap(), tbl.clone());
        cases.push(Case {
            name: "runtime_regclass",
            min_version: 0,
            search_path: "early, late, public",
            setup_sql: vec![
                "CREATE SCHEMA early; CREATE SCHEMA late;".into(),
                "CREATE TABLE late.n (id int); CREATE TABLE late.other (id int);".into(),
                "CREATE VIEW late.v_dyn_ident AS SELECT to_regclass(n::text) AS r FROM (SELECT 'n'::text AS n) q;".into(),
                "CREATE VIEW late.v_dyn AS SELECT to_regclass('n') AS r, 1 AS x WHERE 'n' <> '';".into(),
                "CREATE VIEW late.v_other AS SELECT id FROM late.other;".into(),
            ],
            base,
            desired,
            changes: vec![Change::Table(TableChange::CreateTable {
                uid: Uid::generate(UidKind::Table),
                name: "early.n".parse().unwrap(),
                table: Box::new(tbl),
                beside_default: None,
                moves_at_most: None,
            })],
            change_sql: vec!["CREATE TABLE early.n (id int);".into()],
            tracked: vec![
                TrackedSurface::kept_same(
                    mod_surface("late.v_dyn_ident"),
                    "DROP VIEW late.v_dyn_ident",
                    "CREATE VIEW late.v_dyn_ident AS SELECT to_regclass(n::text) AS r FROM (SELECT 'n'::text AS n) q",
                    Expect::Resolve,
                ),
                TrackedSurface::kept_same(
                    mod_surface("late.v_other"),
                    "DROP VIEW late.v_other",
                    "CREATE VIEW late.v_other AS SELECT id FROM late.other",
                    Expect::UnaffectedByDep,
                ),
            ],
        });
    }

    // 11. arriving functions on a column name (`note`) and a type name (`dom`).
    {
        let mut base = Schema::default();
        base.tables.insert(
            "app.t".parse().unwrap(),
            plain_table(&[("id", "integer"), ("note", "text")]),
        );
        base.tables
            .insert("app.u".parse().unwrap(), plain_table(&[("id", "integer")]));
        base.modules.insert(
            "app.v_col".parse().unwrap(),
            view_mod("SELECT note(t) AS r FROM app.t"),
        );
        base.modules.insert(
            "app.v_dom".parse().unwrap(),
            view_mod("SELECT dom(id) AS r FROM app.t"),
        );
        base.modules
            .insert("app.v_u".parse().unwrap(), view_mod("SELECT id FROM app.u"));
        let mut desired = base.clone();
        desired.modules.insert(
            "app.note(app.t)".parse().unwrap(),
            routine_mod(
                ModuleKind::Function,
                "RETURNS text LANGUAGE sql RETURN 'fn'",
            ),
        );
        desired.modules.insert(
            "app.dom(integer)".parse().unwrap(),
            routine_mod(ModuleKind::Function, "RETURNS int LANGUAGE sql RETURN 99"),
        );
        cases.push(Case {
            name: "fn_on_column_and_type",
            min_version: 0,
            search_path: "app, public",
            setup_sql: vec![
                "CREATE SCHEMA app;".into(),
                "CREATE DOMAIN app.dom AS int;".into(),
                "CREATE TABLE app.t (id int, note text); CREATE TABLE app.u (id int);".into(),
                "CREATE VIEW app.v_col AS SELECT note(t) AS r FROM app.t;".into(),
                "CREATE VIEW app.v_dom AS SELECT dom(id) AS r FROM app.t;".into(),
                "CREATE VIEW app.v_u AS SELECT id FROM app.u;".into(),
            ],
            base,
            desired,
            changes: vec![
                Change::Module(ModuleChange::CreateModule {
                    id: "app.note(app.t)".parse().unwrap(),
                    module: Box::new(routine_mod(
                        ModuleKind::Function,
                        "RETURNS text LANGUAGE sql RETURN 'fn'",
                    )),
                }),
                Change::Module(ModuleChange::CreateModule {
                    id: "app.dom(integer)".parse().unwrap(),
                    module: Box::new(routine_mod(
                        ModuleKind::Function,
                        "RETURNS int LANGUAGE sql RETURN 99",
                    )),
                }),
            ],
            change_sql: vec![
                "CREATE FUNCTION app.note(app.t) RETURNS text LANGUAGE sql RETURN 'fn';".into(),
                "CREATE FUNCTION app.dom(int) RETURNS int LANGUAGE sql RETURN 99;".into(),
            ],
            tracked: vec![
                TrackedSurface::kept_same(
                    mod_surface("app.v_col"),
                    "DROP VIEW app.v_col",
                    "CREATE VIEW app.v_col AS SELECT note(t) AS r FROM app.t",
                    Expect::Resolve,
                ),
                TrackedSurface::kept_same(
                    mod_surface("app.v_dom"),
                    "DROP VIEW app.v_dom",
                    "CREATE VIEW app.v_dom AS SELECT dom(id) AS r FROM app.t",
                    Expect::Resolve,
                ),
                TrackedSurface::kept_same(
                    mod_surface("app.v_u"),
                    "DROP VIEW app.v_u",
                    "CREATE VIEW app.v_u AS SELECT id FROM app.u",
                    Expect::UnaffectedByDep,
                ),
            ],
        });
    }

    // 12. inherited table (`c INHERITS (p)`).
    {
        let mut base = Schema::default();
        base.tables
            .insert("app.p".parse().unwrap(), plain_table(&[("id", "integer")]));
        base.tables.insert(
            "app.c".parse().unwrap(),
            plain_table(&[("id", "integer"), ("extra", "text")]),
        );
        base.tables
            .insert("app.u".parse().unwrap(), plain_table(&[("id", "integer")]));
        base.modules.insert(
            "app.v_child".parse().unwrap(),
            view_mod("SELECT * FROM app.c"),
        );
        base.modules
            .insert("app.v_u".parse().unwrap(), view_mod("SELECT id FROM app.u"));
        let mut desired = base.clone();
        desired
            .tables
            .get_mut(&"app.p".parse().unwrap())
            .unwrap()
            .columns
            .insert("n".into(), Column::new("integer".parse().unwrap()));
        cases.push(Case {
            name: "inherited_table",
            min_version: 0,
            search_path: "app, public",
            setup_sql: vec![
                "CREATE SCHEMA app;".into(),
                "CREATE TABLE app.p (id int);".into(),
                "CREATE TABLE app.c (extra text) INHERITS (app.p);".into(),
                "CREATE TABLE app.u (id int);".into(),
                "CREATE VIEW app.v_child AS SELECT * FROM app.c;".into(),
                "CREATE VIEW app.v_u AS SELECT id FROM app.u;".into(),
            ],
            base,
            desired,
            changes: vec![Change::Column(ColumnChange::AddColumn {
                uid: Uid::generate(UidKind::Column),
                table: "app.p".parse().unwrap(),
                name: "n".into(),
                column: Box::new(Column::new("integer".parse().unwrap())),
            })],
            change_sql: vec!["ALTER TABLE app.p ADD COLUMN n int;".into()],
            tracked: vec![
                TrackedSurface::kept_same(
                    mod_surface("app.v_child"),
                    "DROP VIEW app.v_child",
                    "CREATE VIEW app.v_child AS SELECT * FROM app.c",
                    Expect::Resolve,
                ),
                TrackedSurface::kept_same(
                    mod_surface("app.v_u"),
                    "DROP VIEW app.v_u",
                    "CREATE VIEW app.v_u AS SELECT id FROM app.u",
                    Expect::UnaffectedByDep,
                ),
            ],
        });
    }

    // 13. partition (`pt1 PARTITION OF pt`).
    {
        let mut base = Schema::default();
        base.tables.insert(
            "app.pt".parse().unwrap(),
            plain_table(&[("id", "integer"), ("k", "integer")]),
        );
        base.tables.insert(
            "app.pt1".parse().unwrap(),
            plain_table(&[("id", "integer"), ("k", "integer")]),
        );
        base.tables
            .insert("app.u".parse().unwrap(), plain_table(&[("id", "integer")]));
        base.modules.insert(
            "app.v_part".parse().unwrap(),
            view_mod("SELECT * FROM app.pt1"),
        );
        base.modules
            .insert("app.v_u".parse().unwrap(), view_mod("SELECT id FROM app.u"));
        let mut desired = base.clone();
        desired
            .tables
            .get_mut(&"app.pt".parse().unwrap())
            .unwrap()
            .columns
            .insert("n".into(), Column::new("integer".parse().unwrap()));
        cases.push(Case {
            name: "partition",
            min_version: 0,
            search_path: "app, public",
            setup_sql: vec![
                "CREATE SCHEMA app;".into(),
                "CREATE TABLE app.pt (id int, k int) PARTITION BY RANGE (id);".into(),
                "CREATE TABLE app.pt1 PARTITION OF app.pt FOR VALUES FROM (0) TO (100);".into(),
                "CREATE TABLE app.u (id int);".into(),
                "CREATE VIEW app.v_part AS SELECT * FROM app.pt1;".into(),
                "CREATE VIEW app.v_u AS SELECT id FROM app.u;".into(),
            ],
            base,
            desired,
            changes: vec![Change::Column(ColumnChange::AddColumn {
                uid: Uid::generate(UidKind::Column),
                table: "app.pt".parse().unwrap(),
                name: "n".into(),
                column: Box::new(Column::new("integer".parse().unwrap())),
            })],
            change_sql: vec!["ALTER TABLE app.pt ADD COLUMN n int;".into()],
            tracked: vec![
                TrackedSurface::kept_same(
                    mod_surface("app.v_part"),
                    "DROP VIEW app.v_part",
                    "CREATE VIEW app.v_part AS SELECT * FROM app.pt1",
                    Expect::Resolve,
                ),
                TrackedSurface::kept_same(
                    mod_surface("app.v_u"),
                    "DROP VIEW app.v_u",
                    "CREATE VIEW app.v_u AS SELECT id FROM app.u",
                    Expect::UnaffectedByDep,
                ),
            ],
        });
    }

    // 14. typed table (`ot OF comp`).
    {
        let mut base = Schema::default();
        base.tables
            .insert("app.ot".parse().unwrap(), plain_table(&[("a", "integer")]));
        base.tables
            .insert("app.u".parse().unwrap(), plain_table(&[("id", "integer")]));
        base.tables.insert(
            "app.unrelated".parse().unwrap(),
            plain_table(&[("id", "integer")]),
        );
        base.modules.insert(
            "app.v_typed".parse().unwrap(),
            view_mod("SELECT * FROM app.ot"),
        );
        base.modules
            .insert("app.v_u".parse().unwrap(), view_mod("SELECT id FROM app.u"));
        let desired = base.clone();
        cases.push(Case {
            name: "typed_table",
            min_version: 0,
            search_path: "app, public",
            setup_sql: vec![
                "CREATE SCHEMA app;".into(),
                "CREATE TYPE app.comp AS (a int);".into(),
                "CREATE TABLE app.ot OF app.comp;".into(),
                "CREATE TABLE app.u (id int); CREATE TABLE app.unrelated (id int);".into(),
                "CREATE VIEW app.v_typed AS SELECT * FROM app.ot;".into(),
                "CREATE VIEW app.v_u AS SELECT id FROM app.u;".into(),
            ],
            base,
            desired,
            changes: vec![
                // Pair the type touch with a moving change on an unrelated table so
                // the plan enters the per-surface narrowing path.
                Change::Column(ColumnChange::AddColumn {
                    uid: Uid::generate(UidKind::Column),
                    table: "app.unrelated".parse().unwrap(),
                    name: "x".into(),
                    column: Box::new(Column::new("integer".parse().unwrap())),
                }),
                Change::Type(TypeChange::DropType {
                    uid: Uid::generate(UidKind::Type),
                    name: "app.comp".parse().unwrap(),
                    definition: Box::new(ManagedType::Enum {
                        values: vec!["a".into()],
                    }),
                }),
            ],
            change_sql: vec![
                "ALTER TABLE app.unrelated ADD COLUMN x int;".into(),
                "ALTER TYPE app.comp ADD ATTRIBUTE n int CASCADE;".into(),
            ],
            tracked: vec![
                TrackedSurface::kept_same(
                    mod_surface("app.v_typed"),
                    "DROP VIEW app.v_typed",
                    "CREATE VIEW app.v_typed AS SELECT * FROM app.ot",
                    Expect::Resolve,
                ),
                TrackedSurface::kept_same(
                    mod_surface("app.v_u"),
                    "DROP VIEW app.v_u",
                    "CREATE VIEW app.v_u AS SELECT id FROM app.u",
                    Expect::UnaffectedByDep,
                ),
            ],
        });
    }

    // 15. multi-object surfaces (view column, default, check, expression+partial index, trigger)
    //     each beside an unrelated neighbour surface.
    {
        let mut base = Schema::default();
        let mut tbl = plain_table(&[("id", "integer")]);
        let mut d_hit = Column::new("integer".parse().unwrap());
        d_hit.value_source = ValueSource::Default("fn7(0)".into());
        tbl.columns.insert("d_hit".into(), d_hit);
        let mut d_ok = Column::new("integer".parse().unwrap());
        d_ok.value_source = ValueSource::Default("42".into());
        tbl.columns.insert("d_ok".into(), d_ok);
        tbl.checks.insert(
            "ck_hit".into(),
            CheckConstraint {
                expression: "fn7(id) > 0".into(),
            },
        );
        tbl.checks.insert(
            "ck_ok".into(),
            CheckConstraint {
                expression: "id >= 0".into(),
            },
        );
        tbl.indexes.insert(
            "ix_hit".into(),
            Index {
                columns: vec![IndexColumn {
                    key: IndexKey::Expression("(fn7(id))".into()),
                    descending: false,
                    opclass: None,
                    collation: None,
                    opclass_parameters: BTreeMap::new(),
                }],
                include: Vec::new(),
                unique: false,
                filter: Some("fn7(id) > 0".into()),
                method: Default::default(),
                storage_parameters: Default::default(),
                filegroup: None,
            },
        );
        tbl.indexes.insert(
            "ix_ok".into(),
            Index {
                columns: vec![IndexColumn {
                    key: IndexKey::Expression("(id + 1)".into()),
                    descending: false,
                    opclass: None,
                    collation: None,
                    opclass_parameters: BTreeMap::new(),
                }],
                include: Vec::new(),
                unique: false,
                filter: Some("id > 0".into()),
                method: Default::default(),
                storage_parameters: Default::default(),
                filegroup: None,
            },
        );
        let t_name: TableName = "app.t".parse().unwrap();
        base.tables.insert(t_name.clone(), tbl);
        base.modules.insert(
            ModuleId::Trigger {
                on: t_name.clone(),
                name: "tg_hit".into(),
            },
            trigger_mod(
                "BEFORE INSERT ON app.t FOR EACH ROW WHEN (fn7(NEW.id) > 0) EXECUTE FUNCTION app.noop_tg()",
            ),
        );
        base.modules.insert(
            ModuleId::Trigger {
                on: t_name.clone(),
                name: "tg_ok".into(),
            },
            trigger_mod("BEFORE INSERT ON app.t FOR EACH ROW EXECUTE FUNCTION app.noop_tg()"),
        );
        base.modules.insert(
            "app.v_col_hit".parse().unwrap(),
            view_mod("SELECT (fn7(0))::late.vcol_dom AS c"),
        );
        base.modules.insert(
            "app.v_col_ok".parse().unwrap(),
            view_mod("SELECT 1::int AS c"),
        );
        let mut desired = base.clone();
        desired.modules.insert(
            "early.fn7(integer)".parse().unwrap(),
            routine_mod(
                ModuleKind::Function,
                "RETURNS int IMMUTABLE LANGUAGE sql RETURN 2",
            ),
        );
        cases.push(Case {
            name: "multi_object_surfaces",
            min_version: 0,
            search_path: "early, late, app, public",
            setup_sql: vec![
                "CREATE SCHEMA early; CREATE SCHEMA late; CREATE SCHEMA app;".into(),
                "CREATE FUNCTION late.fn7(numeric) RETURNS int IMMUTABLE LANGUAGE sql RETURN 1;".into(),
                "CREATE FUNCTION app.noop_tg() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RETURN NEW; END; $$;".into(),
                "CREATE TABLE app.t (id int, d_hit int DEFAULT fn7(0), d_ok int DEFAULT 42, \
                   CONSTRAINT ck_hit CHECK (fn7(id) > 0), CONSTRAINT ck_ok CHECK (id >= 0));".into(),
                "CREATE DOMAIN late.vcol_dom AS int;".into(),
                "CREATE VIEW app.v_col_hit AS SELECT (fn7(0))::late.vcol_dom AS c;".into(),
                "CREATE VIEW app.v_col_ok AS SELECT 1::int AS c;".into(),
                "CREATE INDEX ix_hit ON app.t ((fn7(id))) WHERE fn7(id) > 0;".into(),
                "CREATE INDEX ix_ok ON app.t ((id + 1)) WHERE id > 0;".into(),
                "CREATE TRIGGER tg_hit BEFORE INSERT ON app.t FOR EACH ROW WHEN (fn7(NEW.id) > 0) EXECUTE FUNCTION app.noop_tg();".into(),
                "CREATE TRIGGER tg_ok BEFORE INSERT ON app.t FOR EACH ROW EXECUTE FUNCTION app.noop_tg();".into(),
            ],
            base,
            desired,
            changes: vec![Change::Module(ModuleChange::CreateModule {
                id: "early.fn7(integer)".parse().unwrap(),
                module: Box::new(routine_mod(
                    ModuleKind::Function,
                    "RETURNS int IMMUTABLE LANGUAGE sql RETURN 2",
                )),
            })],
            change_sql: vec![
                "CREATE FUNCTION early.fn7(int) RETURNS int IMMUTABLE LANGUAGE sql RETURN 2;".into(),
            ],
            tracked: vec![
                TrackedSurface::kept_same(
                    Surface::Default(t_name.column("d_hit")),
                    "ALTER TABLE app.t ALTER COLUMN d_hit DROP DEFAULT",
                    "ALTER TABLE app.t ALTER COLUMN d_hit SET DEFAULT fn7(0)",
                    Expect::Resolve,
                ),
                TrackedSurface::kept_same(
                    Surface::Default(t_name.column("d_ok")),
                    "ALTER TABLE app.t ALTER COLUMN d_ok DROP DEFAULT",
                    "ALTER TABLE app.t ALTER COLUMN d_ok SET DEFAULT 42",
                    Expect::UnaffectedByDep,
                ),
                TrackedSurface::kept_same(
                    Surface::Check {
                        table: t_name.clone(),
                        name: "ck_hit".into(),
                    },
                    "ALTER TABLE app.t DROP CONSTRAINT ck_hit",
                    "ALTER TABLE app.t ADD CONSTRAINT ck_hit CHECK (fn7(id) > 0)",
                    Expect::Resolve,
                ),
                TrackedSurface::kept_same(
                    Surface::Check {
                        table: t_name.clone(),
                        name: "ck_ok".into(),
                    },
                    "ALTER TABLE app.t DROP CONSTRAINT ck_ok",
                    "ALTER TABLE app.t ADD CONSTRAINT ck_ok CHECK (id >= 0)",
                    Expect::UnaffectedByDep,
                ),
                TrackedSurface::kept_same(
                    Surface::Index {
                        table: t_name.clone(),
                        name: "ix_hit".into(),
                    },
                    "DROP INDEX app.ix_hit",
                    "CREATE INDEX ix_hit ON app.t ((fn7(id))) WHERE fn7(id) > 0",
                    Expect::Resolve,
                ),
                TrackedSurface::kept_same(
                    Surface::Index {
                        table: t_name.clone(),
                        name: "ix_ok".into(),
                    },
                    "DROP INDEX app.ix_ok",
                    "CREATE INDEX ix_ok ON app.t ((id + 1)) WHERE id > 0",
                    Expect::UnaffectedByDep,
                ),
                TrackedSurface::kept_same(
                    Surface::Module(ModuleId::Trigger {
                        on: t_name.clone(),
                        name: "tg_hit".into(),
                    }),
                    "DROP TRIGGER tg_hit ON app.t",
                    "CREATE TRIGGER tg_hit BEFORE INSERT ON app.t FOR EACH ROW WHEN (fn7(NEW.id) > 0) EXECUTE FUNCTION app.noop_tg()",
                    Expect::Resolve,
                ),
                TrackedSurface::kept_same(
                    Surface::Module(ModuleId::Trigger {
                        on: t_name,
                        name: "tg_ok".into(),
                    }),
                    "DROP TRIGGER tg_ok ON app.t",
                    "CREATE TRIGGER tg_ok BEFORE INSERT ON app.t FOR EACH ROW EXECUTE FUNCTION app.noop_tg()",
                    Expect::UnaffectedByDep,
                ),
                TrackedSurface::kept_same(
                    mod_surface("app.v_col_hit"),
                    "DROP VIEW app.v_col_hit",
                    "CREATE VIEW app.v_col_hit AS SELECT (fn7(0))::late.vcol_dom AS c",
                    Expect::Resolve,
                ),
                TrackedSurface::kept_same(
                    mod_surface("app.v_col_ok"),
                    "DROP VIEW app.v_col_ok",
                    "CREATE VIEW app.v_col_ok AS SELECT 1::int AS c",
                    Expect::UnaffectedByDep,
                ),
            ],
        });
    }

    // 16a. arriving table `early.t` brings array type `early._t` (free) capturing `late._t`.
    {
        let mut base = Schema::default();
        base.tables.insert(
            "late.other".parse().unwrap(),
            plain_table(&[("id", "integer")]),
        );
        base.modules.insert(
            "app.v_arr".parse().unwrap(),
            view_mod("SELECT NULL::_t AS r"),
        );
        base.modules.insert(
            "app.v_other".parse().unwrap(),
            view_mod("SELECT id FROM late.other"),
        );
        let mut desired = base.clone();
        let tbl = plain_table(&[("id", "integer")]);
        desired
            .tables
            .insert("early.t".parse().unwrap(), tbl.clone());
        cases.push(Case {
            name: "array_type_free",
            min_version: 0,
            search_path: "early, late, app, public",
            setup_sql: vec![
                "CREATE SCHEMA early; CREATE SCHEMA late; CREATE SCHEMA app;".into(),
                "CREATE DOMAIN late._t AS integer;".into(),
                "CREATE TABLE late.other (id int);".into(),
                "CREATE VIEW app.v_arr AS SELECT NULL::_t AS r;".into(),
                "CREATE VIEW app.v_other AS SELECT id FROM late.other;".into(),
            ],
            base,
            desired,
            changes: vec![Change::Table(TableChange::CreateTable {
                uid: Uid::generate(UidKind::Table),
                name: "early.t".parse().unwrap(),
                table: Box::new(tbl),
                beside_default: None,
                moves_at_most: None,
            })],
            change_sql: vec!["CREATE TABLE early.t (id int);".into()],
            tracked: vec![
                TrackedSurface::kept_same(
                    mod_surface("app.v_arr"),
                    "DROP VIEW app.v_arr",
                    "CREATE VIEW app.v_arr AS SELECT NULL::_t AS r",
                    Expect::Resolve,
                ),
                TrackedSurface::kept_same(
                    mod_surface("app.v_other"),
                    "DROP VIEW app.v_other",
                    "CREATE VIEW app.v_other AS SELECT id FROM late.other",
                    Expect::UnaffectedByDep,
                ),
            ],
        });
    }

    // 16b. first choice held: `early._t` is already held so `early.t` takes `early.__t`, capturing `late.__t`.
    {
        let mut base = Schema::default();
        base.modules.insert(
            "app.v_fallback".parse().unwrap(),
            view_mod("SELECT NULL::__t AS r"),
        );
        let mut desired = base.clone();
        let tbl = plain_table(&[("id", "integer")]);
        desired
            .tables
            .insert("early.t".parse().unwrap(), tbl.clone());
        cases.push(Case {
            name: "array_type_held",
            min_version: 0,
            search_path: "early, late, app, public",
            setup_sql: vec![
                "CREATE SCHEMA early; CREATE SCHEMA late; CREATE SCHEMA app;".into(),
                "CREATE DOMAIN early._t AS integer;".into(),
                "CREATE DOMAIN late.__t AS integer;".into(),
                "CREATE VIEW app.v_fallback AS SELECT NULL::__t AS r;".into(),
            ],
            base,
            desired,
            changes: vec![Change::Table(TableChange::CreateTable {
                uid: Uid::generate(UidKind::Table),
                name: "early.t".parse().unwrap(),
                table: Box::new(tbl),
                beside_default: None,
                moves_at_most: None,
            })],
            change_sql: vec!["CREATE TABLE early.t (id int);".into()],
            tracked: vec![TrackedSurface::kept_same(
                mod_surface("app.v_fallback"),
                "DROP VIEW app.v_fallback",
                "CREATE VIEW app.v_fallback AS SELECT NULL::__t AS r",
                Expect::Resolve,
            )],
        });
    }

    // 17. Owned sequence + table moved across schemas (`ALTER TABLE app.t SET SCHEMA early`).
    {
        let mut base = Schema::default();
        let mut tbl = plain_table(&[("id", "integer")]);
        let mut seq_col = Column::new("integer".parse().unwrap());
        seq_col.value_source = ValueSource::Sequence(OwnedSequence {
            name: Some("carried_seq".into()),
            ..OwnedSequence::serial()
        });
        tbl.columns.insert("n".into(), seq_col);
        base.tables.insert("app.t".parse().unwrap(), tbl.clone());
        base.tables
            .insert("app.u".parse().unwrap(), plain_table(&[("id", "integer")]));
        base.modules.insert(
            "app.v_shadowed_seq".parse().unwrap(),
            view_mod("SELECT 'carried_seq'::regclass AS r"),
        );
        base.modules
            .insert("app.v_u".parse().unwrap(), view_mod("SELECT id FROM app.u"));
        let mut desired = base.clone();
        desired.tables.remove(&"app.t".parse().unwrap());
        desired.tables.insert("early.t".parse().unwrap(), tbl);
        cases.push(Case {
            name: "owned_sequence_moved_across_schemas",
            min_version: 0,
            search_path: "early, late, app, public",
            setup_sql: vec![
                "CREATE SCHEMA early; CREATE SCHEMA late; CREATE SCHEMA app;".into(),
                "CREATE SEQUENCE late.carried_seq;".into(),
                "CREATE TABLE app.t (id int, n int);".into(),
                "CREATE SEQUENCE app.carried_seq OWNED BY app.t.n;".into(),
                "CREATE TABLE app.u (id int);".into(),
                "CREATE VIEW app.v_shadowed_seq AS SELECT 'carried_seq'::regclass AS r;".into(),
                "CREATE VIEW app.v_u AS SELECT id FROM app.u;".into(),
            ],
            base,
            desired,
            changes: vec![Change::Table(TableChange::RenameTable {
                uid: Uid::generate(UidKind::Table),
                from: "app.t".parse().unwrap(),
                to: "early.t".parse().unwrap(),
                defaults: Vec::new(),
            })],
            change_sql: vec!["ALTER TABLE app.t SET SCHEMA early;".into()],
            tracked: vec![
                TrackedSurface::kept_same(
                    mod_surface("app.v_shadowed_seq"),
                    "DROP VIEW app.v_shadowed_seq",
                    "CREATE VIEW app.v_shadowed_seq AS SELECT 'carried_seq'::regclass AS r",
                    Expect::Resolve,
                ),
                TrackedSurface::kept_same(
                    mod_surface("app.v_u"),
                    "DROP VIEW app.v_u",
                    "CREATE VIEW app.v_u AS SELECT id FROM app.u",
                    Expect::UnaffectedByDep,
                ),
            ],
        });
    }

    // 18. Unparsed routine with a parameter default calling a function whose overload arrives.
    {
        let mut base = Schema::default();
        base.tables
            .insert("app.u".parse().unwrap(), plain_table(&[("id", "integer")]));
        let f_hit_def = "(x integer DEFAULT g(1)) RETURNS integer LANGUAGE plpgsql AS $$ BEGIN RETURN x; END; $$";
        let f_ok_def = "() RETURNS integer LANGUAGE plpgsql AS $$ BEGIN RETURN 1; END; $$";
        base.modules.insert(
            "app.f_pl(integer)".parse().unwrap(),
            routine_mod(ModuleKind::Function, f_hit_def),
        );
        base.modules.insert(
            "app.f_pl_other()".parse().unwrap(),
            routine_mod(ModuleKind::Function, f_ok_def),
        );
        base.modules
            .insert("app.v_u".parse().unwrap(), view_mod("SELECT id FROM app.u"));
        let mut desired = base.clone();
        desired.modules.insert(
            "early.g(integer)".parse().unwrap(),
            routine_mod(ModuleKind::Function, "RETURNS int LANGUAGE sql RETURN 2"),
        );
        cases.push(Case {
            name: "unparsed_routine_header_default",
            min_version: 0,
            search_path: "early, late, app, public",
            setup_sql: vec![
                "CREATE SCHEMA early; CREATE SCHEMA late; CREATE SCHEMA app;".into(),
                "CREATE TABLE app.u (id int);".into(),
                "CREATE FUNCTION late.g(numeric) RETURNS int LANGUAGE sql RETURN 1;".into(),
                format!("CREATE FUNCTION app.f_pl{f_hit_def};"),
                format!("CREATE FUNCTION app.f_pl_other{f_ok_def};"),
                "CREATE VIEW app.v_u AS SELECT id FROM app.u;".into(),
            ],
            base,
            desired,
            changes: vec![Change::Module(ModuleChange::CreateModule {
                id: "early.g(integer)".parse().unwrap(),
                module: Box::new(routine_mod(
                    ModuleKind::Function,
                    "RETURNS int LANGUAGE sql RETURN 2",
                )),
            })],
            change_sql: vec![
                "CREATE FUNCTION early.g(int) RETURNS int LANGUAGE sql RETURN 2;".into(),
            ],
            tracked: vec![
                TrackedSurface::kept_same(
                    mod_surface("app.f_pl(integer)"),
                    "DROP FUNCTION app.f_pl(integer)",
                    format!("CREATE FUNCTION app.f_pl{f_hit_def}"),
                    Expect::Resolve,
                ),
                TrackedSurface::kept_same(
                    mod_surface("app.f_pl_other()"),
                    "DROP FUNCTION app.f_pl_other()",
                    format!("CREATE FUNCTION app.f_pl_other{f_ok_def}"),
                    Expect::UnaffectedUnparsed,
                ),
                TrackedSurface::kept_same(
                    mod_surface("app.v_u"),
                    "DROP VIEW app.v_u",
                    "CREATE VIEW app.v_u AS SELECT id FROM app.u",
                    Expect::UnaffectedByDep,
                ),
            ],
        });
    }

    cases
}

async fn run_case(
    server: &str,
    version: i64,
    case: &Case,
    rows: &mut Vec<String>,
) -> Result<(), String> {
    if version < case.min_version {
        rows.push(format!(
            "{} | {} | (skipped: requires PG {}) | - | -",
            version, case.name, case.min_version
        ));
        return Ok(());
    }
    let mut admin = Conn::connect(Driver::Postgres, server).await.unwrap();
    let db_name = format!("pbps_df2987_{}_{}", case.name, std::process::id());
    admin
        .execute(&format!("DROP DATABASE IF EXISTS {db_name} WITH (FORCE)"))
        .await
        .unwrap();
    admin
        .execute(&format!("CREATE DATABASE {db_name} TEMPLATE template0"))
        .await
        .unwrap();
    let own_url = server.replace("dbname=pbps_test", &format!("dbname={db_name}"));
    let url = if own_url == server {
        format!("{server} dbname={db_name}")
    } else {
        own_url
    };
    let result = run_case_on(&url, version, case, rows).await;
    let _ = admin
        .execute(&format!("DROP DATABASE IF EXISTS {db_name} WITH (FORCE)"))
        .await;
    result
}

async fn run_case_on(
    url: &str,
    version: i64,
    case: &Case,
    rows: &mut Vec<String>,
) -> Result<(), String> {
    let mut conn = Conn::connect(Driver::Postgres, url)
        .await
        .map_err(|e| e.to_string())?;
    conn.execute(&format!("SET search_path = {}", case.search_path))
        .await
        .map_err(|e| e.to_string())?;
    for stmt in &case.setup_sql {
        conn.execute(stmt)
            .await
            .map_err(|e| format!("{}: setup `{stmt}` failed: {e}", case.name))?;
    }

    // The capture sets a transaction-local search path and uses a savepoint,
    // so it runs inside a transaction that is rolled back.
    let mut before_sigs = BTreeMap::new();
    conn.execute("BEGIN").await.map_err(|e| e.to_string())?;
    for tracked in &case.tracked {
        let sig = capture_binding(&mut conn, &tracked.base)
            .await
            .map_err(|e| format!("{}: before capture {:?}: {e}", case.name, tracked.base))?;
        before_sigs.insert(tracked.base.clone(), sig);
    }
    conn.execute("ROLLBACK").await.map_err(|e| e.to_string())?;

    let pg = Postgres::new();
    let base_ids = resolve_ids(&case.base, &IdsFile::default(), &[]);
    // A table the case renames keeps its identity, as a recorded rename does.
    let intents: Vec<pbps_model::Intent> = case
        .changes
        .iter()
        .filter_map(|change| {
            if let Change::Table(TableChange::RenameTable { from, to, .. }) = change {
                Some(pbps_model::Intent::RenameTable {
                    from: from.clone(),
                    to: to.clone(),
                })
            } else {
                None
            }
        })
        .collect();
    let desired_ids = resolve_ids(&case.desired, &base_ids, &intents);
    let base_side = pbps_diff::Side {
        schema: &case.base,
        ids: &base_ids,
    };
    let desired_side = pbps_diff::Side {
        schema: &case.desired,
        ids: &desired_ids,
    };
    let ordinary = ChangeSet {
        changes: case
            .changes
            .iter()
            .cloned()
            .map(PlannedChange::new)
            .collect(),
    };

    let needed = needed_facts(base_side, desired_side, &ordinary, &pg);
    let facts = match needed.as_ref() {
        Some(n) => Some(
            pbps_pg::resolver::facts::read(&mut conn, n)
                .await
                .map_err(|e| format!("{}: facts::read failed: {e}", case.name))?,
        ),
        None => None,
    };
    let assessment = assess_with_facts(base_side, desired_side, &ordinary, &pg, facts.as_ref());

    // Apply the change and ask the engine directly by recreating each kept
    // surface inside a rolled-back transaction under the case's search_path.
    conn.execute(&format!("SET search_path = {}", case.search_path))
        .await
        .map_err(|e| e.to_string())?;
    for stmt in &case.change_sql {
        conn.execute(stmt)
            .await
            .map_err(|e| format!("{}: change `{stmt}` failed: {e}", case.name))?;
    }

    for tracked in &case.tracked {
        let answer = *assessment
            .questions
            .get(&tracked.kept)
            .ok_or_else(|| format!("{}: missing assessment for {:?}", case.name, tracked.kept))?;
        let reason = assessment.unaffected_reason(&tracked.kept);
        conn.execute("BEGIN").await.map_err(|e| e.to_string())?;
        let recreate_res: Result<BindingSnapshot, String> = async {
            conn.execute(&format!("SET LOCAL search_path = {}", case.search_path))
                .await
                .map_err(|e| e.to_string())?;
            conn.execute(&tracked.drop_sql)
                .await
                .map_err(|e| e.to_string())?;
            conn.execute(&tracked.recreate_sql)
                .await
                .map_err(|e| e.to_string())?;
            capture_binding(&mut conn, &tracked.kept).await
        }
        .await;
        conn.execute("ROLLBACK").await.map_err(|e| e.to_string())?;
        let rebound = match recreate_res {
            Ok(after_sig) => after_sig != before_sigs[&tracked.base],
            Err(_) => true,
        };
        rows.push(format!(
            "pg{version} | {} | {} | {answer:?} ({reason:?}) | {}",
            case.name,
            surface_label(&tracked.kept),
            if rebound { "yes" } else { "no" },
        ));
        if reason == Some(UnaffectedReason::ByDependency) && rebound {
            return Err(format!(
                "COUNTER-EXAMPLE on pg{version} in {}: {} was answered Unaffected(ByDependency) but rebound!",
                case.name,
                surface_label(&tracked.kept)
            ));
        }
        match tracked.expect {
            Expect::Resolve => {
                if answer != Answer::Resolve {
                    return Err(format!(
                        "pg{version} {}: expected Resolve for {}, got {answer:?} ({reason:?})",
                        case.name,
                        surface_label(&tracked.kept)
                    ));
                }
            }
            Expect::UnaffectedByDep => {
                if answer != Answer::Unaffected || reason != Some(UnaffectedReason::ByDependency) {
                    return Err(format!(
                        "pg{version} {}: expected Unaffected(ByDependency) for {}, got {answer:?} ({reason:?})",
                        case.name,
                        surface_label(&tracked.kept)
                    ));
                }
            }
            Expect::UnaffectedUnparsed => {
                if answer != Answer::Unaffected || reason != Some(UnaffectedReason::Unparsed) {
                    return Err(format!(
                        "pg{version} {}: expected Unaffected(Unparsed) for {}, got {answer:?} ({reason:?})",
                        case.name,
                        surface_label(&tracked.kept)
                    ));
                }
            }
        }
    }
    Ok(())
}

#[tokio::test]
#[ignore = "needs live PostgreSQL 16 and 18; set PBPS_TEST_PG_OLD_DB and PBPS_TEST_PG_DB"]
async fn differential_dependency_facts_never_answer_unaffected_when_a_surface_rebinds() {
    let cases = corpus();
    let mut failures = Vec::new();
    let mut table = vec![
        "server | case | surface | answer | rebound".to_owned(),
        "---|---|---|---|---".to_owned(),
    ];
    for var in SERVERS {
        let server = std::env::var(var)
            .unwrap_or_else(|_| panic!("{var} must name a pinned PostgreSQL server"));
        let mut conn = Conn::connect(Driver::Postgres, &server).await.unwrap();
        let version = pbps_pg::roles::server_version_num(&mut conn).await.unwrap();
        drop(conn);
        for case in &cases {
            if let Err(why) = run_case(&server, version, case, &mut table).await {
                failures.push(why);
            }
        }
    }
    eprintln!("\n{}\n", table.join("\n"));
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
