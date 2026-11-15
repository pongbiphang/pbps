//! `depends_on:` of the typed prefixes against a live engine (#2469 D,
//! DEC-2469.8).
//!
//! The offline tests (`depends_on_yaml.rs`) show that each prefix orders its
//! part in the plan. These show that the engine then runs that order: the
//! script puts the function first, the build succeeds, and `verify` finds no
//! drift. A prefix whose engine path only a live run can show has its own
//! case: a sequence rides the CREATE TABLE group, a policy and row-level
//! security follow every module, an exclusion constraint is its own step, and
//! a view's index is held with the whole view.
use super::{Demo, OwnDatabase, code, on_server, plan_checksum, stderr, stdout};
use std::process::Output;

fn rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

fn scalar(pg: bool, connection: &str, sql: &str) -> i64 {
    rt().block_on(async {
        let driver = if pg {
            pbps_db::Driver::Postgres
        } else {
            pbps_db::Driver::Mssql
        };
        let mut c = pbps_db::Conn::connect(driver, connection).await.unwrap();
        c.query(sql).await.unwrap()[0]
            .try_get_at::<i64>(0)
            .unwrap()
            .unwrap()
    })
}

fn ok(o: Output) -> Output {
    assert_eq!(code(&o), 0, "{}{}", stdout(&o), stderr(&o));
    o
}

fn said(o: &Output) -> String {
    format!("{}{}", stdout(o), stderr(o))
}

/// The function every case points at. Nothing names it, so only the
/// annotation can put it first.
fn function(pg: bool, depends_on: &str) -> String {
    if pg {
        format!(
            "function: app.f()\n{depends_on}definition: |-\n  () RETURNS integer LANGUAGE sql AS $$ SELECT 1 $$\n"
        )
    } else {
        format!(
            "function: dbo.zf\n{depends_on}definition: |-\n  () RETURNS int AS BEGIN RETURN 1 END\n"
        )
    }
}

fn target(pg: bool) -> &'static str {
    if pg {
        "module::app.f()"
    } else {
        "module::dbo.zf"
    }
}

/// An empty database and a project built into it by `bootstrap`; returns what
/// the build ran, in order.
fn build(pg: bool, server: &str, slug: &str, files: &[(&str, String)]) -> (Demo, OwnDatabase, String) {
    let own = OwnDatabase::new(server, slug);
    if pg {
        on_server(own.connection(), "CREATE SCHEMA app");
    }
    let d = Demo::new(slug);
    for (name, body) in files {
        std::fs::write(d.dir.join("schema").join(name), body).unwrap();
    }
    // The identities of the declarations, which a typed edge binds to.
    ok(d.run(&["plan", "--no-dev"]));
    d.commit();
    let script = d.dir.join("build.sql");
    ok(d.run(&["bootstrap", "--sql", script.to_str().unwrap()]));
    ok(d.run(&["bootstrap", "--db", own.connection()]));
    ok(d.run(&["verify", "--db", own.connection()]));
    let script = std::fs::read_to_string(&script).unwrap();
    (d, own, script)
}

fn at(script: &str, text: &str) -> usize {
    script
        .find(text)
        .unwrap_or_else(|| panic!("`{text}` is not in:\n{script}"))
}

/// A serial column's sequence is created with its table, so an annotation on
/// the sequence holds the whole CREATE TABLE group behind the function
/// (decision 8.4 = A).
pub fn a_sequence_annotation_holds_the_create_table_group(server: &str) {
    let table = |depends_on: &str| {
        format!("table: app.t\n{depends_on}columns:\n  id: {{type: serial}}\n")
    };
    let annotated = table(&format!(
        "depends_on:\n  \"sequence::app.t.id\": [\"{}\"]\n",
        target(true)
    ));
    let (_d, own, script) = build(
        true,
        server,
        "dep-sequence",
        &[("app.f.yml", function(true, "")), ("app.t.yml", annotated)],
    );
    assert!(at(&script, "CREATE FUNCTION") < at(&script, "CREATE SEQUENCE"), "{script}");
    assert!(at(&script, "CREATE SEQUENCE") < at(&script, "CREATE TABLE"), "{script}");
    assert_eq!(
        scalar(true, own.connection(), "SELECT count(*) FROM pg_class WHERE relkind = 'S' AND relname = 't_id_seq'"),
        1
    );
    // Negative: without the annotation the table is first.
    let (_d, _own, plain) = build(
        true,
        server,
        "dep-sequence-plain",
        &[("app.f.yml", function(true, "")), ("app.t.yml", table(""))],
    );
    assert!(at(&plain, "CREATE TABLE") < at(&plain, "CREATE FUNCTION"), "{plain}");
}

/// A policy and row-level security are installed after every module, so an
/// owner annotation holds nothing back, and asking a module to follow one is
/// refused naming both legs.
pub fn policy_and_row_security_annotations_order_the_policy_install_and_refuse_the_reverse(server: &str) {
    let table = |using: &str| {
        format!(
            "table: app.t\ndepends_on:\n  \"policy::app.t.p\": [\"{f}\"]\n  \"row_security::app.t\": [\"{f}\"]\ncolumns:\n  id: {{type: int, nullable: false}}\nrow_security: {{enabled: true}}\nrow_policies:\n  p: {{using: \"{using}\"}}\n",
            f = target(true)
        )
    };
    let (d, own, script) = build(
        true,
        server,
        "dep-policy",
        &[("app.f.yml", function(true, "")), ("app.t.yml", table("true"))],
    );
    assert!(at(&script, "CREATE FUNCTION") < at(&script, "CREATE POLICY"), "{script}");
    assert!(at(&script, "CREATE POLICY") < at(&script, "ENABLE ROW LEVEL SECURITY"), "{script}");
    let connection = own.connection();
    assert_eq!(scalar(true, connection, "SELECT count(*) FROM pg_policies WHERE policyname = 'p'"), 1);
    assert_eq!(scalar(true, connection, "SELECT count(*) FROM pg_class WHERE relname = 't' AND relrowsecurity"), 1);
    // The reverse: the function follows the policy. Both change, so both are
    // steps, and the plan names the two that wait on one another.
    std::fs::write(
        d.dir.join("schema/app.f.yml"),
        function(true, "depends_on: [\"policy::app.t.p\"]\n").replace("SELECT 1", "SELECT 2"),
    )
    .unwrap();
    std::fs::write(d.dir.join("schema/app.t.yml"), table("false")).unwrap();
    let refused = d.run(&["plan", "--db", connection]);
    assert_ne!(code(&refused), 0, "{}", said(&refused));
    let text = said(&refused);
    assert!(text.contains("wait on one another") && text.contains("app.f()"), "{text}");
}

/// An exclusion constraint is added by a step of its own, after its table's
/// rows, so the annotation puts it behind the function.
pub fn an_exclusion_annotation_orders_the_extracted_constraint(server: &str) {
    let table = format!(
        "table: app.t\ndepends_on:\n  \"exclusion::app.t.e\": [\"{}\"]\ncolumns:\n  id: {{type: int, nullable: false}}\n  x: {{type: int}}\nexclude:\n  e:\n    method: btree\n    keys:\n      - {{column: x, operator: \"=\"}}\n",
        target(true)
    );
    let (_d, own, script) = build(
        true,
        server,
        "dep-exclusion",
        &[("app.f.yml", function(true, "")), ("app.t.yml", table)],
    );
    assert!(at(&script, "CREATE FUNCTION") < at(&script, "EXCLUDE USING btree"), "{script}");
    assert!(at(&script, "CREATE TABLE") < at(&script, "EXCLUDE USING btree"), "{script}");
    assert_eq!(
        scalar(true, own.connection(), "SELECT count(*) FROM pg_constraint WHERE conname = 'e' AND contype = 'x'"),
        1
    );
}

/// A view's index is not a step of its own: the annotation holds the whole
/// view behind the function (decision 8.4 = A).
pub fn a_view_index_annotation_holds_the_whole_view_group(server: &str) {
    let table = "table: dbo.t\ncolumns:\n  id: {type: int, nullable: false}\n";
    let view = |depends_on: &str| {
        format!(
            "view: dbo.v\nschemabinding: true\n{depends_on}indexes:\n  ix: {{unique: true, columns: [id]}}\nclustered: ix\ndefinition: |-\n  SELECT id FROM dbo.t\n"
        )
    };
    let annotated = view(&format!(
        "depends_on:\n  \"index::module::dbo.v.ix\": [\"{}\"]\n",
        target(false)
    ));
    let (_d, own, script) = build(
        false,
        server,
        "dep-view-index",
        &[
            ("dbo.zf.yml", function(false, "")),
            ("dbo.t.yml", table.to_owned()),
            ("dbo.v.yml", annotated),
        ],
    );
    assert!(at(&script, "FUNCTION [dbo].[zf]") < at(&script, "VIEW [dbo].[v]"), "{script}");
    assert_eq!(
        scalar(
            false,
            own.connection(),
            "SELECT count(*) FROM sys.indexes WHERE name = 'ix' AND object_id = OBJECT_ID('dbo.v')"
        ),
        1
    );
    // Negative: the function sorts after the view, so without the annotation
    // the view is built first.
    let (_d, _own, plain) = build(
        false,
        server,
        "dep-view-index-plain",
        &[
            ("dbo.zf.yml", function(false, "")),
            ("dbo.t.yml", table.to_owned()),
            ("dbo.v.yml", view("")),
        ],
    );
    assert!(at(&plain, "VIEW [dbo].[v]") < at(&plain, "FUNCTION [dbo].[zf]"), "{plain}");
}

/// The ordinary deployment route as well as the build: a table already in the
/// database takes a function and three annotated parts, the plan puts the
/// function first and the parts after it, `apply` runs it, and nothing drifts.
pub fn a_project_with_typed_depends_on_applies_in_the_declared_order(server: &str, pg: bool) {
    let schema = if pg { "app" } else { "dbo" };
    let table = |annotated: bool| {
        let depends_on = if annotated {
            format!(
                "depends_on:\n  \"default::{schema}.t.x\": [\"{f}\"]\n  \"check::{schema}.t.c\": [\"{f}\"]\n  \"unique::{schema}.t.u\": [\"{f}\"]\n",
                f = target(pg)
            )
        } else {
            String::new()
        };
        let parts = if annotated {
            "checks:\n  c: \"x >= 0\"\nunique:\n  u: {columns: [x]}\n"
        } else {
            ""
        };
        let default = if annotated { ", default: \"0\"" } else { "" };
        format!(
            "table: {schema}.t\n{depends_on}columns:\n  id: {{type: int, nullable: false}}\n  x: {{type: int{default}}}\n{parts}"
        )
    };
    let file = if pg { "app.t.yml" } else { "dbo.t.yml" };
    let (d, own, _) = build(pg, server, "dep-ordinary", &[(file, table(false))]);
    let connection = own.connection();
    let function_file = if pg { "app.f.yml" } else { "dbo.zf.yml" };
    std::fs::write(d.dir.join("schema").join(function_file), function(pg, "")).unwrap();
    std::fs::write(d.dir.join("schema").join(file), table(true)).unwrap();
    // The new function's identity, then the plan against the database.
    ok(d.run(&["plan", "--no-dev"]));
    d.commit();
    let plan = d.dir.join("plan.json");
    let sql = d.dir.join("plan.sql");
    ok(d.run(&[
        "plan",
        "--db",
        connection,
        "--out",
        plan.to_str().unwrap(),
        "--sql",
        sql.to_str().unwrap(),
    ]));
    let script = std::fs::read_to_string(&sql).unwrap();
    let function_at = at(&script, if pg { "CREATE FUNCTION" } else { "FUNCTION [dbo].[zf]" });
    for part in [
        if pg { "SET DEFAULT" } else { "DEFAULT (0" },
        if pg { "ADD CONSTRAINT \"c\" CHECK" } else { "ADD CONSTRAINT [c] CHECK" },
        if pg { "ADD CONSTRAINT \"u\" UNIQUE" } else { "ADD CONSTRAINT [u] UNIQUE" },
    ] {
        assert!(function_at < at(&script, part), "`{part}` is before the function:\n{script}");
    }
    let applied = d.run(&[
        "apply",
        "--db",
        connection,
        "--plan",
        plan.to_str().unwrap(),
        "--checksum",
        &plan_checksum(&plan),
        "--allow",
        "constraint,narrowing",
    ]);
    ok(applied);
    let next = ok(d.run(&["plan", "--db", connection]));
    assert!(stdout(&next).contains("No changes"), "{}", stdout(&next));
    ok(d.run(&["verify", "--db", connection]));
}
