//! DEFAULT is b_expr in a column constraint and CREATE DOMAIN (DEC-2312.1).
use super::*;

// The grammar failures plus ordinary defaults, comments and existing grouping.
// Expected values are SQL expressions so date/time display settings cannot
// make an otherwise correct default look different.
const CASES: &[(&str, &str, &str, &str)] = &[
    ("in", "boolean", "'a' IN ('a','b')", "true"),
    ("is_null", "boolean", "NULL IS NULL", "true"),
    ("between", "boolean", "1 BETWEEN 0 AND 2", "true"),
    ("like", "boolean", "'abc' LIKE 'a%'", "true"),
    ("and", "boolean", "true AND false", "false"),
    ("or", "boolean", "false OR true", "true"),
    ("not", "boolean", "NOT false", "true"),
    ("is_true", "boolean", "true IS TRUE", "true"),
    ("ilike", "boolean", "'Abc' ILIKE 'a%'", "true"),
    ("similar", "boolean", "'abc' SIMILAR TO 'a%'", "true"),
    ("any", "boolean", "'a' = ANY (ARRAY['a','b'])", "true"),
    (
        "at_time_zone",
        "timestamp with time zone",
        "TIMESTAMP '2026-01-02 03:04:05' AT TIME ZONE 'UTC'",
        "TIMESTAMPTZ '2026-01-02 03:04:05+00'",
    ),
    ("comparison", "boolean", "1 < 2", "true"),
    ("arithmetic", "integer", "1 + 2", "3"),
    ("ordinary_integer", "integer", "7", "7"),
    ("ordinary_text", "text", "'plain'", "'plain'"),
    ("line_comment", "integer", "1 -- why", "1"),
    ("already_grouped", "boolean", "('a' IN ('a','b'))", "true"),
    ("distinct", "boolean", "1 IS DISTINCT FROM 2", "true"),
    ("collate", "text", "'a' COLLATE \"C\"", "'a'"),
    ("function", "integer", "pg_catalog.abs(-2)", "2"),
];

fn columns(domains: bool) -> String {
    let mut text = "table: app.t\ncolumns:\n  id: {type: integer}\n".to_owned();
    for (name, ty, expression, _) in CASES {
        if domains {
            text.push_str(&format!("  v_{name}: {{type: app.d_{name}}}\n"));
        } else {
            text.push_str(&format!(
                "  v_{name}:\n    type: {ty}\n    default: {expression:?}\n"
            ));
        }
    }
    text
}

fn planned(d: &Demo, db: &str) -> (PathBuf, pbps_model::SavedPlan) {
    let path = d.dir.join("defaults.plan.json");
    succeeds(d.run(&["plan", "--db", db, "--out", path.to_str().unwrap()]));
    let plan = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    (path, plan)
}

fn applied(d: &Demo, db: &str) {
    succeeds(d.run(&["plan", "--no-dev"]));
    d.commit();
    let (path, plan) = planned(d, db);
    assert!(!plan.changes.is_empty());
    succeeds(apply_plan(d, db, &path, false));
    succeeds(d.run(&["verify", "--db", db]));
    assert!(planned(d, db).1.changes.is_empty());
}

fn values_and_collation(db: &str) {
    assert!(scalar(db, "SELECT count(*)::bigint FROM app.t") > 0);
    for (name, _, _, expected) in CASES {
        assert_eq!(
            scalar(
                db,
                &format!(
                    "SELECT count(*)::bigint FROM app.t WHERE v_{name} IS DISTINCT FROM {expected}"
                )
            ),
            0,
            "default {name}"
        );
    }
    // Before grouping, the COLLATE was consumed as a column attribute, not
    // as part of the default. Its collation must stay the declared default.
    assert_eq!(
        scalar(
            db,
            "SELECT count(*)::bigint FROM pg_attribute WHERE attrelid='app.t'::regclass \
             AND attname='v_collate' AND attcollation=(SELECT typcollation FROM pg_type \
             WHERE oid='text'::regtype)"
        ),
        1
    );
}

fn declared_columns_are_unchanged(db: &str) {
    let snapshot = latest_snapshot(db);
    let defaults = &snapshot.declared.expressions.defaults[&pbps_model::TableName::new("app", "t")];
    for (name, _, expression, _) in CASES {
        assert_eq!(
            defaults.get(&pbps_model::ColumnName::from(format!("v_{name}"))),
            Some(&expression.to_string())
        );
    }
    assert_eq!(
        text_of(
            db,
            "SELECT pg_get_expr(adbin,adrelid) FROM pg_attrdef d JOIN pg_attribute a \
             ON a.attrelid=d.adrelid AND a.attnum=d.adnum WHERE d.adrelid='app.t'::regclass \
             AND a.attname='v_ordinary_integer'"
        ),
        "7"
    );
}

fn created_columns(server: &str) {
    let own = OwnDatabase::new(server, "defaults2312_create");
    let db = own.connection();
    on_server(db, "CREATE SCHEMA app");
    let d = Demo::new("defaults2312-create");
    d.commit();
    succeeds(d.run(&["baseline", "--db", db, "--reason", "empty database"]));
    d.table(&columns(false));
    applied(&d, db);
    on_server(db, "INSERT INTO app.t DEFAULT VALUES");
    values_and_collation(db);
    declared_columns_are_unchanged(db);
    assert!(planned(&d, db).1.changes.is_empty());
}

#[test]
#[ignore = "needs live PostgreSQL 16"]
fn created_column_defaults_accept_full_expressions_and_converge_on_16() {
    created_columns(&old_server());
}

#[test]
#[ignore = "needs live PostgreSQL 18"]
fn created_column_defaults_accept_full_expressions_and_converge_on_18() {
    created_columns(&server());
}

fn added_columns(server: &str) {
    let own = OwnDatabase::new(server, "defaults2312_add");
    let db = own.connection();
    on_server(db, "CREATE SCHEMA app");
    let d = Demo::new("defaults2312-add");
    d.table("table: app.t\ncolumns:\n  id: {type: integer}\n");
    succeeds(d.run(&["plan", "--no-dev"]));
    d.commit();
    succeeds(d.run(&["bootstrap", "--db", db]));
    on_server(db, "INSERT INTO app.t VALUES (1)");
    d.table(&columns(false));
    applied(&d, db);
    on_server(db, "INSERT INTO app.t DEFAULT VALUES");
    assert_eq!(scalar(db, "SELECT count(*)::bigint FROM app.t"), 2);
    values_and_collation(db);
    declared_columns_are_unchanged(db);
    assert!(planned(&d, db).1.changes.is_empty());
}

#[test]
#[ignore = "needs live PostgreSQL 16"]
fn added_column_defaults_accept_full_expressions_and_converge_on_16() {
    added_columns(&old_server());
}

#[test]
#[ignore = "needs live PostgreSQL 18"]
fn added_column_defaults_accept_full_expressions_and_converge_on_18() {
    added_columns(&server());
}

fn domains(server: &str) {
    let own = OwnDatabase::new(server, "defaults2312_domain");
    let db = own.connection();
    on_server(db, "CREATE SCHEMA app");
    let d = Demo::new("defaults2312-domain");
    d.commit();
    succeeds(d.run(&["baseline", "--db", db, "--reason", "empty database"]));
    for (name, ty, expression, _) in CASES {
        std::fs::write(
            d.dir.join(format!("schema/app.d_{name}.type.yml")),
            format!("domain: app.d_{name}\ntype: {ty}\ndefault: {expression:?}\n"),
        )
        .unwrap();
    }
    d.table(&columns(true));
    applied(&d, db);
    on_server(db, "INSERT INTO app.t DEFAULT VALUES");
    values_and_collation(db);
    let snapshot = latest_snapshot(db);
    assert_eq!(
        snapshot.declared.types,
        pbps_load::load_schema_dir(&d.dir.join("schema"))
            .unwrap()
            .schema
            .types
    );
    assert_eq!(
        scalar(
            db,
            "SELECT count(*)::bigint FROM pg_type WHERE oid='app.d_collate'::regtype \
             AND typcollation=(SELECT typcollation FROM pg_type WHERE oid='text'::regtype)"
        ),
        1
    );
    assert!(planned(&d, db).1.changes.is_empty());
}

#[test]
#[ignore = "needs live PostgreSQL 16"]
fn domain_defaults_accept_full_expressions_and_converge_on_16() {
    domains(&old_server());
}

#[test]
#[ignore = "needs live PostgreSQL 18"]
fn domain_defaults_accept_full_expressions_and_converge_on_18() {
    domains(&server());
}
