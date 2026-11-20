//! `depends_on:` written in YAML reaches the plan for every kind of reference
//! (#2469 D, DEC-2469.8).
//!
//! The scenarios hide the dependency from the identifier scan on purpose: no
//! declaration names the function the annotation points at, so the order can
//! only come from the annotation. Each case is run twice, without the
//! annotation and with it, and the two must differ; that difference is what
//! shows the annotation did the ordering.

use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicUsize, Ordering};

const BIN: &str = env!("CARGO_BIN_EXE_pbps");

struct Project {
    root: PathBuf,
}

impl Project {
    fn new(dialect: &str) -> Self {
        // One directory per project: the tests run in parallel.
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let root = std::env::temp_dir().join(format!(
            "pbps-depends-on-yaml-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("schema")).unwrap();
        std::fs::write(root.join("pbps.yml"), format!("dialect: {dialect}\n")).unwrap();
        let p = Self { root };
        p.git(&["init", "-q"]);
        p.git(&["config", "user.email", "d@e.f"]);
        p.git(&["config", "user.name", "demo"]);
        p
    }

    fn git(&self, args: &[&str]) {
        let out = Command::new("git")
            .arg("-C")
            .arg(&self.root)
            .args(args)
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?} failed");
    }

    fn file(&self, name: &str, body: &str) {
        std::fs::write(self.root.join("schema").join(name), body).unwrap();
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(BIN)
            .arg("--project")
            .arg(&self.root)
            .args(args)
            .output()
            .unwrap()
    }

    fn said(&self, args: &[&str]) -> (Option<i32>, String) {
        let o = self.run(args);
        let text =
            String::from_utf8_lossy(&o.stdout).to_string() + &String::from_utf8_lossy(&o.stderr);
        (o.status.code(), text)
    }

    /// The plan, as the steps it orders (`op target`, in order), and how many
    /// edges it records; or what the refusal said.
    fn plan(&self) -> Result<(Vec<String>, usize), String> {
        let out = self.root.join("plan.json");
        let _ = std::fs::remove_file(&out);
        let (code, text) = self.said(&["plan", "--out", out.to_str().unwrap()]);
        if code != Some(0) {
            return Err(text);
        }
        let plan: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&out).unwrap()).unwrap();
        let steps = plan["changes"]["changes"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|c| c["op"] != "public_execution")
            .map(|c| {
                let target = c["id"].as_str().or(c["name"].as_str()).unwrap_or("");
                format!("{} {target}", c["op"].as_str().unwrap())
                    .trim()
                    .to_owned()
            })
            .collect();
        let edges = plan["object_deps"].as_array().map_or(0, Vec::len);
        Ok((steps, edges))
    }
}

impl Drop for Project {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// What the annotated project must do.
enum Then {
    /// Plan, in this order.
    Order(&'static [&'static str]),
    /// Refuse, with this said.
    Refuse(&'static str),
}

struct Case {
    name: &'static str,
    dialect: &'static str,
    /// The files of the project, the annotated one as it reads without the
    /// annotation.
    files: Vec<(&'static str, String)>,
    /// The file that takes the annotation, whole, with it.
    annotated: (&'static str, String),
    /// The order without the annotation.
    plain: &'static [&'static str],
    then: Then,
}

/// A PostgreSQL project: a function nobody names, a table `app.t`.
const PG_F: &str =
    "function: app.f()\n{dep}definition: |-\n  () RETURNS integer LANGUAGE sql AS $$ SELECT 1 $$\n";
/// The same for SQL Server, which names a function without its arguments.
const MS_F: &str = "function: dbo.f\n{dep}definition: |-\n  () RETURNS int AS BEGIN RETURN 1 END\n";

fn table(schema: &str, dep: &str, x: &str, rest: &str) -> String {
    format!(
        "table: {schema}.t\n{dep}columns:\n  id: {{type: int, nullable: false}}\n  x: {{type: int{x}}}\n{rest}"
    )
}

/// A table whose part `key` must follow the function.
fn part(schema: &str, key: &str, f: &str, x: &str, rest: &str) -> (String, String) {
    (
        table(schema, "", x, rest),
        table(
            schema,
            &format!("depends_on:\n  \"{key}\": [\"module::{f}\"]\n"),
            x,
            rest,
        ),
    )
}

fn pg(
    name: &'static str,
    plain: &'static [&'static str],
    key: &str,
    x: &str,
    rest: &str,
    then: &'static [&'static str],
) -> Case {
    let (plain_table, annotated) = part("app", key, "app.f()", x, rest);
    Case {
        name,
        dialect: "postgres",
        files: vec![
            ("app.f.yml", PG_F.replace("{dep}", "")),
            ("app.t.yml", plain_table),
        ],
        annotated: ("app.t.yml", annotated),
        plain,
        then: Then::Order(then),
    }
}

fn ms(
    name: &'static str,
    plain: &'static [&'static str],
    key: &str,
    x: &str,
    rest: &str,
    then: &'static [&'static str],
) -> Case {
    let (plain_table, annotated) = part("dbo", key, "dbo.f", x, rest);
    Case {
        name,
        dialect: "mssql",
        files: vec![
            ("dbo.f.yml", MS_F.replace("{dep}", "")),
            ("dbo.t.yml", plain_table),
        ],
        annotated: ("dbo.t.yml", annotated),
        plain,
        then: Then::Order(then),
    }
}

const PG_BASE: &[&str] = &["create_table app.t", "create_module app.f()"];
const MS_BASE: &[&str] = &["create_table dbo.t", "create_module dbo.f"];

fn pg_cases() -> Vec<Case> {
    let function_first = &["create_module app.f()", "create_table app.t"];
    let mut cases = vec![
        // The root: a list on the table, and each part that rides the
        // CREATE TABLE group boundary.
        Case {
            name: "table",
            then: Then::Order(function_first),
            annotated: (
                "app.t.yml",
                table("app", "depends_on: [\"module::app.f()\"]\n", "", ""),
            ),
            ..pg("table", PG_BASE, "", "", "", &[])
        },
        pg(
            "column",
            PG_BASE,
            "column::app.t.x",
            "",
            "",
            &[
                "create_table app.t",
                "create_module app.f()",
                "add_column x",
            ],
        ),
        pg(
            "default",
            PG_BASE,
            "default::app.t.x",
            ", default: \"0\"",
            "",
            &[
                "create_table app.t",
                "create_module app.f()",
                "alter_column_default",
            ],
        ),
        pg(
            "generation",
            PG_BASE,
            "generation::app.t.x",
            ", generated: {expression: \"id + 1\", stored: true}",
            "",
            &[
                "create_table app.t",
                "create_module app.f()",
                "add_column x",
            ],
        ),
        pg(
            "not_null",
            PG_BASE,
            "not_null::app.t.x",
            ", nullable: false",
            "",
            &[
                "create_table app.t",
                "create_module app.f()",
                "alter_column_nullability",
            ],
        ),
        pg(
            "index",
            PG_BASE,
            "index::app.t.ix",
            "",
            "indexes:\n  ix: {columns: [x]}\n",
            &[
                "create_table app.t",
                "create_module app.f()",
                "add_index ix",
            ],
        ),
        pg(
            "unique",
            PG_BASE,
            "unique::app.t.u",
            "",
            "unique:\n  u: {columns: [x]}\n",
            &[
                "create_table app.t",
                "create_module app.f()",
                "add_unique u",
            ],
        ),
        pg(
            "check",
            PG_BASE,
            "check::app.t.c",
            "",
            "checks:\n  c: \"x > 0\"\n",
            &["create_table app.t", "create_module app.f()", "add_check c"],
        ),
        pg(
            "primary_key",
            PG_BASE,
            "primary_key::app.t",
            "",
            "primary_key: [id]\n",
            &[
                "create_table app.t",
                "create_module app.f()",
                "set_primary_key",
            ],
        ),
        pg(
            "exclusion",
            &[
                "create_table app.t",
                "set_exclusion e",
                "create_module app.f()",
            ],
            "exclusion::app.t.e",
            "",
            "exclude:\n  e:\n    method: btree\n    keys:\n      - {column: x, operator: \"=\"}\n",
            &[
                "create_table app.t",
                "create_module app.f()",
                "set_exclusion e",
            ],
        ),
    ];
    // A serial column's sequence is created with its table: the annotation
    // holds the whole CREATE TABLE group (decision 8.4 = A).
    let serial = "table: app.t\n{dep}columns:\n  id: {type: serial}\n";
    cases.push(Case {
        name: "sequence",
        dialect: "postgres",
        files: vec![
            ("app.f.yml", PG_F.replace("{dep}", "")),
            ("app.t.yml", serial.replace("{dep}", "")),
        ],
        annotated: (
            "app.t.yml",
            serial.replace(
                "{dep}",
                "depends_on:\n  \"sequence::app.t.id\": [\"module::app.f()\"]\n",
            ),
        ),
        plain: PG_BASE,
        then: Then::Order(function_first),
    });
    // A foreign key needs the table it points at.
    let parent = "table: app.p\ncolumns:\n  id: {type: int, nullable: false}\nprimary_key: [id]\n"
        .to_owned();
    let (plain_table, annotated) = part(
        "app",
        "foreign_key::app.t.fk",
        "app.f()",
        "",
        "foreign_keys:\n  fk: {columns: [x], references: app.p(id)}\n",
    );
    cases.push(Case {
        name: "foreign_key",
        dialect: "postgres",
        files: vec![
            ("app.f.yml", PG_F.replace("{dep}", "")),
            ("app.p.yml", parent),
            ("app.t.yml", plain_table),
        ],
        annotated: ("app.t.yml", annotated),
        plain: &[
            "create_table app.p",
            "create_table app.t",
            "add_foreign_key fk",
            "create_module app.f()",
        ],
        then: Then::Order(&[
            "create_table app.p",
            "create_table app.t",
            "create_module app.f()",
            "add_foreign_key fk",
        ]),
    });
    // An enum and a domain, whole and by part.
    let enumeration = "enum: app.mood\n{dep}values: [a, b]\n";
    cases.push(Case {
        name: "type (enum)",
        dialect: "postgres",
        files: vec![
            ("app.f.yml", PG_F.replace("{dep}", "")),
            ("app.mood.yml", enumeration.replace("{dep}", "")),
        ],
        annotated: (
            "app.mood.yml",
            enumeration.replace("{dep}", "depends_on: [\"module::app.f()\"]\n"),
        ),
        plain: &["create_type app.mood", "create_module app.f()"],
        then: Then::Order(&["create_module app.f()", "create_type app.mood"]),
    });
    let domain = "domain: app.d\n{dep}type: int\nnot_null: true\ndefault: \"1\"\nchecks:\n  c: \"VALUE > 0\"\n";
    cases.push(Case {
        name: "type (domain)",
        dialect: "postgres",
        files: vec![
            ("app.f.yml", PG_F.replace("{dep}", "")),
            ("app.d.yml", domain.replace("{dep}", "")),
        ],
        annotated: (
            "app.d.yml",
            domain.replace("{dep}", "depends_on: [\"module::app.f()\"]\n"),
        ),
        plain: &["create_type app.d", "create_module app.f()"],
        then: Then::Order(&["create_module app.f()", "create_type app.d"]),
    });
    for key in [
        "check::type::app.d.c",
        "default::type::app.d",
        "not_null::type::app.d",
    ] {
        cases.push(Case {
            name: Box::leak(format!("type (domain part) {key}").into_boxed_str()),
            dialect: "postgres",
            files: vec![
                ("app.f.yml", PG_F.replace("{dep}", "")),
                ("app.d.yml", domain.replace("{dep}", "")),
            ],
            annotated: (
                "app.d.yml",
                domain.replace(
                    "{dep}",
                    &format!("depends_on:\n  \"{key}\": [\"module::app.f()\"]\n"),
                ),
            ),
            plain: &["create_type app.d", "create_module app.f()"],
            then: Then::Order(&[
                "create_type app.d",
                "create_module app.f()",
                "alter_domain app.d",
            ]),
        });
    }
    // A policy and row-level security are installed after every module, so a
    // module cannot be made to follow one: that is refused, with its legs.
    for (name, key, rest) in [
        (
            "policy",
            "policy::app.t.p",
            "row_security: {enabled: true}\nrow_policies:\n  p: {using: \"true\"}\n",
        ),
        (
            "row_security",
            "row_security::app.t",
            "row_security: {enabled: true}\n",
        ),
    ] {
        let function = PG_F.replace("{dep}", &format!("depends_on: [\"{key}\"]\n"));
        cases.push(Case {
            name,
            dialect: "postgres",
            files: vec![
                ("app.f.yml", PG_F.replace("{dep}", "")),
                ("app.t.yml", table("app", "", "", rest)),
            ],
            annotated: ("app.f.yml", function),
            plain: if name == "policy" {
                &[
                    "create_table app.t",
                    "create_module app.f()",
                    "create_policy p",
                    "set_row_security",
                ]
            } else {
                &[
                    "create_table app.t",
                    "create_module app.f()",
                    "set_row_security",
                ]
            },
            then: Then::Refuse("these steps wait on one another"),
        });
    }
    // The role is provisioned outside the baseline on PostgreSQL: there is no
    // step that creates it, so there is nothing to order against.
    let role = "role: r1\n{dep}grants:\n  app.t: [select]\n";
    cases.push(Case {
        name: "role",
        dialect: "postgres",
        files: vec![
            ("app.f.yml", PG_F.replace("{dep}", "")),
            ("app.t.yml", table("app", "", "", "")),
            ("r1.yml", role.replace("{dep}", "")),
        ],
        annotated: (
            "r1.yml",
            role.replace("{dep}", "depends_on: [\"module::app.f()\"]\n"),
        ),
        plain: &["create_table app.t", "create_module app.f()", "grant"],
        then: Then::Refuse("provision the role before the baseline"),
    });
    cases
}

fn ms_cases() -> Vec<Case> {
    let function_first = &["create_module dbo.f", "create_table dbo.t"];
    let mut cases = vec![
        Case {
            annotated: (
                "dbo.t.yml",
                table("dbo", "depends_on: [\"module::dbo.f\"]\n", "", ""),
            ),
            then: Then::Order(function_first),
            ..ms("table", MS_BASE, "", "", "", &[])
        },
        ms(
            "column",
            MS_BASE,
            "column::dbo.t.x",
            "",
            "",
            &["create_table dbo.t", "create_module dbo.f", "add_column x"],
        ),
        ms(
            "default",
            MS_BASE,
            "default::dbo.t.x",
            ", default: \"0\"",
            "",
            &[
                "create_table dbo.t",
                "create_module dbo.f",
                "alter_column_default",
            ],
        ),
        ms(
            "not_null",
            MS_BASE,
            "not_null::dbo.t.x",
            ", nullable: false",
            "",
            &[
                "create_table dbo.t",
                "create_module dbo.f",
                "alter_column_nullability",
            ],
        ),
        ms(
            "index",
            MS_BASE,
            "index::dbo.t.ix",
            "",
            "indexes:\n  ix: {columns: [x]}\n",
            &["create_table dbo.t", "create_module dbo.f", "add_index ix"],
        ),
        ms(
            "unique",
            MS_BASE,
            "unique::dbo.t.u",
            "",
            "unique:\n  u: {columns: [x]}\n",
            &["create_table dbo.t", "create_module dbo.f", "add_unique u"],
        ),
        ms(
            "check",
            MS_BASE,
            "check::dbo.t.c",
            "",
            "checks:\n  c: \"x > 0\"\n",
            &["create_table dbo.t", "create_module dbo.f", "add_check c"],
        ),
        ms(
            "primary_key",
            MS_BASE,
            "primary_key::dbo.t",
            "",
            "primary_key: [id]\n",
            &[
                "create_table dbo.t",
                "create_module dbo.f",
                "set_primary_key",
            ],
        ),
        // SQL Server's generated column is a computed one.
        ms(
            "generation (computed)",
            MS_BASE,
            "generation::dbo.t.y",
            "",
            "computed:\n  y: {expression: \"x * 2\"}\n",
            &[
                "create_table dbo.t",
                "create_module dbo.f",
                "add_computed_column y",
            ],
        ),
    ];
    let parent = "table: dbo.p\ncolumns:\n  id: {type: int, nullable: false}\nprimary_key: [id]\n"
        .to_owned();
    let (plain_table, annotated) = part(
        "dbo",
        "foreign_key::dbo.t.fk",
        "dbo.f",
        "",
        "foreign_keys:\n  fk: {columns: [x], references: dbo.p(id)}\n",
    );
    cases.push(Case {
        name: "foreign_key",
        dialect: "mssql",
        files: vec![
            ("dbo.f.yml", MS_F.replace("{dep}", "")),
            ("dbo.p.yml", parent),
            ("dbo.t.yml", plain_table),
        ],
        annotated: ("dbo.t.yml", annotated),
        plain: &[
            "create_table dbo.p",
            "create_table dbo.t",
            "add_foreign_key fk",
            "create_module dbo.f",
        ],
        then: Then::Order(&[
            "create_table dbo.p",
            "create_table dbo.t",
            "create_module dbo.f",
            "add_foreign_key fk",
        ]),
    });
    // An indexed view's index is held with the whole view (decision 8.4 = A).
    // The function sorts after the view, so only the annotation puts it first.
    let view = "view: dbo.v\nschemabinding: true\n{dep}indexes:\n  ix: {unique: true, columns: [id]}\nclustered: ix\ndefinition: |-\n  SELECT id FROM dbo.t\n";
    cases.push(Case {
        name: "index (view)",
        dialect: "mssql",
        files: vec![
            (
                "dbo.zf.yml",
                MS_F.replace("{dep}", "").replace("dbo.f", "dbo.zf"),
            ),
            ("dbo.t.yml", table("dbo", "", "", "")),
            ("dbo.v.yml", view.replace("{dep}", "")),
        ],
        annotated: (
            "dbo.v.yml",
            view.replace(
                "{dep}",
                "depends_on:\n  \"index::module::dbo.v.ix\": [\"module::dbo.zf\"]\n",
            ),
        ),
        plain: &[
            "create_table dbo.t",
            "create_module dbo.v",
            "create_module dbo.zf",
        ],
        then: Then::Order(&[
            "create_table dbo.t",
            "create_module dbo.zf",
            "create_module dbo.v",
        ]),
    });
    // A role is created by a step here, so the function can be put behind it.
    let role = "role: r1\ngrants:\n  dbo.t: [select]\n";
    cases.push(Case {
        name: "role",
        dialect: "mssql",
        files: vec![
            ("dbo.f.yml", MS_F.replace("{dep}", "")),
            ("dbo.t.yml", table("dbo", "", "", "")),
            ("r1.yml", role.to_owned()),
        ],
        annotated: (
            "dbo.f.yml",
            MS_F.replace("{dep}", "depends_on: [\"role::r1\"]\n"),
        ),
        plain: &[
            "create_table dbo.t",
            "create_module dbo.f",
            "create_role r1",
            "grant",
        ],
        then: Then::Order(&[
            "create_table dbo.t",
            "create_role r1",
            "create_module dbo.f",
            "grant",
        ]),
    });
    cases
}

fn run_case(case: &Case) -> Result<(), String> {
    let at = |annotated: bool| {
        let p = Project::new(case.dialect);
        for (name, body) in &case.files {
            p.file(name, body);
        }
        if annotated {
            p.file(case.annotated.0, &case.annotated.1);
        }
        p
    };
    let name = case.name;
    let (plain, no_edges) = at(false)
        .plan()
        .map_err(|why| format!("{name}: the project without the annotation is refused: {why}"))?;
    if plain != case.plain || no_edges != 0 {
        return Err(format!(
            "{name}: without the annotation: {plain:?}, {no_edges} edges"
        ));
    }
    match (&case.then, at(true).plan()) {
        (Then::Order(order), Ok((steps, edges))) => {
            if steps != *order || edges != 1 {
                return Err(format!(
                    "{name}: with the annotation: {steps:?}, {edges} edges"
                ));
            }
            // The two orders differ: that is what shows the annotation did it.
            assert_ne!(steps, plain, "{name}");
            Ok(())
        }
        (Then::Refuse(said), Err(why)) if why.contains(said) => Ok(()),
        (Then::Refuse(said), Err(why)) => {
            Err(format!("{name}: refused, but not for `{said}`: {why}"))
        }
        (Then::Order(_), Err(why)) => Err(format!("{name}: refused: {why}")),
        (Then::Refuse(said), Ok((steps, _))) => {
            Err(format!("{name}: planned {steps:?}, not `{said}`"))
        }
    }
}

#[test]
fn each_typed_prefix_orders_its_part_in_a_plan_from_yaml() {
    let failures: Vec<String> = pg_cases()
        .iter()
        .chain(&ms_cases())
        .filter_map(|case| run_case(case).err())
        .collect();
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn a_prefix_that_only_postgresql_has_is_refused_on_sql_server_for_that_reason() {
    for reference in [
        "sequence::dbo.t.id",
        "type::dbo.mood",
        "policy::dbo.t.p",
        "row_security::dbo.t",
        "exclusion::dbo.t.e",
    ] {
        let p = Project::new("mssql");
        p.file("dbo.t.yml", &table("dbo", "", "", ""));
        let function = MS_F.replace("{dep}", &format!("depends_on: [\"{reference}\"]\n"));
        p.file("dbo.f.yml", &function);
        let (code, text) = p.said(&["validate"]);
        assert_eq!(code, Some(2), "{reference}: {text}");
        assert!(
            text.contains(&format!(
                "`{reference}` names an object only PostgreSQL has"
            )),
            "{reference}: {text}"
        );
        // Negative: the portable spelling of the same project is accepted.
        p.file(
            "dbo.f.yml",
            &MS_F.replace("{dep}", "depends_on: [\"table::dbo.t\"]\n"),
        );
        let (code, text) = p.said(&["validate"]);
        assert_eq!(code, Some(0), "{reference}: {text}");
    }
}

/// A project with `app.f`, and a table whose default is hidden from the scan.
fn with_default_after_function(dependency: &str) -> Project {
    let p = Project::new("postgres");
    p.file("app.f.yml", &PG_F.replace("{dep}", ""));
    p.file(
        "app.t.yml",
        &table("app", dependency, ", default: \"0\"", ""),
    );
    p
}

#[test]
fn a_baseline_revisions_typed_annotation_orders_the_removal_of_what_it_names() {
    // The baseline holds a default that follows the function; the next
    // revision removes both. The default must go first, and only the older
    // revision's annotation says so.
    let removal = |dependency: &str| {
        let p = with_default_after_function(dependency);
        // The first plan gives the declarations their identities, which the
        // commit then carries: a typed edge binds to them.
        p.plan().unwrap();
        p.git(&["add", "-A"]);
        p.git(&["commit", "-qm", "base"]);
        std::fs::remove_file(p.root.join("schema/app.f.yml")).unwrap();
        p.file("app.t.yml", &table("app", "", "", ""));
        p.plan().unwrap().0
    };
    let annotated = removal("depends_on:\n  \"default::app.t.x\": [\"module::app.f()\"]\n");
    assert_eq!(annotated, ["alter_column_default", "drop_module app.f()"]);
    // Negative: a revision that never had the annotation reads as having none.
    let plain = removal("");
    assert_eq!(plain, ["drop_module app.f()", "alter_column_default"]);
}

#[test]
fn a_baseline_revision_with_an_ambiguous_typed_spelling_is_refused() {
    // At the older revision `table::app.t` is a table and also a view's legacy
    // name. Read as the table, it would order a removal against the wrong
    // object, so the revision is refused the way today's files would be.
    let since = |spelling: &str| {
        let p = with_default_after_function("");
        p.file("m.yml", "view: table::app.t\ndefinition: SELECT 1 AS n\n");
        let owner =
            |dependency: &str| format!("view: app.v\n{dependency}definition: SELECT 1 AS n\n");
        p.file("app.v.yml", &owner(""));
        p.plan().unwrap();
        p.git(&["add", "-A"]);
        p.git(&["commit", "-qm", "base"]);
        // The revision under test is committed as written: nothing checked it.
        p.file(
            "app.v.yml",
            &owner(&format!("depends_on: [\"{spelling}\"]\n")),
        );
        p.git(&["commit", "-qam", "annotated"]);
        p.file("app.v.yml", &owner(""));
        p.plan()
    };
    let refusal = since("table::app.t").unwrap_err();
    assert!(
        refusal.contains("the baseline version itself is broken"),
        "{refusal}"
    );
    assert!(
        refusal.contains("`depends_on` item `table::app.t`"),
        "{refusal}"
    );
    assert!(refusal.contains("ambiguous"), "{refusal}");
    // Negative: the spelling that says which was meant is a readable baseline.
    since("module::table::app.t").unwrap();
}

#[test]
fn validate_names_each_typed_dependency_problem_from_yaml() {
    let check = |dependency: &str, expected: &[&str]| {
        let p = with_default_after_function(dependency);
        let (code, text) = p.said(&["validate"]);
        assert_eq!(code, Some(2), "{dependency}: {text}");
        for said in expected {
            assert!(
                text.contains(said),
                "{dependency}: missing `{said}`: {text}"
            );
        }
        // Negative: the same project without the line is valid.
        let ok = with_default_after_function("");
        assert_eq!(ok.said(&["validate"]).0, Some(0));
    };
    // A target nobody declares, and a part nobody declares.
    check(
        "depends_on: [\"table::app.nope\"]\n",
        &["unknown dependency target: table::app.nope"],
    );
    check(
        "depends_on:\n  \"index::app.t.nope\": [\"module::app.f()\"]\n",
        &["the dependency part is not declared: index::app.t.nope"],
    );
    // A selector that does not apply to the root, and an owner nobody declares.
    check(
        "depends_on:\n  \"check::type::app.t.x\": [\"module::app.f()\"]\n",
        &["unknown dependency owner"],
    );
    check(
        "depends_on:\n  \"table::app.ghost\": [\"module::app.f()\"]\n",
        &["unknown dependency owner: table::app.ghost"],
    );
    // A table that follows itself, and a cycle across two kinds.
    check(
        "depends_on: [\"table::app.t\"]\n",
        &["the dependency owner lists itself"],
    );
    let p = with_default_after_function("depends_on: [\"module::app.f()\"]\n");
    p.file(
        "app.f.yml",
        &PG_F.replace("{dep}", "depends_on: [\"table::app.t\"]\n"),
    );
    let (code, text) = p.said(&["validate"]);
    assert_eq!(code, Some(2), "{text}");
    assert!(text.contains("close a cycle"), "{text}");
    assert!(text.contains("table::app.t -> module::app.f()"), "{text}");
}

#[test]
fn a_routine_target_is_spelled_as_the_dialect_spells_it() {
    // PostgreSQL reads `int` as `integer`: the annotation names the function
    // the way the file does, and still finds it.
    let p = Project::new("postgres");
    p.file(
        "app.f.yml",
        &PG_F
            .replace("{dep}", "")
            .replace("function: app.f()", "function: app.f(integer)")
            .replace("() RETURNS", "(integer) RETURNS"),
    );
    p.file(
        "app.t.yml",
        &table("app", "depends_on: [\"module::app.f(int)\"]\n", "", ""),
    );
    let (code, text) = p.said(&["validate"]);
    assert_eq!(code, Some(0), "{text}");
    let (steps, edges) = p.plan().unwrap();
    assert_eq!(
        steps,
        ["create_module app.f(integer)", "create_table app.t"]
    );
    assert_eq!(edges, 1);
    // Negative: a signature that names no declared function is refused.
    p.file(
        "app.t.yml",
        &table("app", "depends_on: [\"module::app.f(text)\"]\n", "", ""),
    );
    let (code, text) = p.said(&["validate"]);
    assert_eq!(code, Some(2), "{text}");
    assert!(text.contains("app.f(text)"), "{text}");
}

#[test]
fn the_structured_escape_loads_formats_to_the_shorthand_and_plans_alike() {
    let escaped =
        "depends_on: ['object::{\"root\":{\"module\":\"app.f()\"},\"part\":\"whole\"}']\n";
    let p = Project::new("postgres");
    p.file("app.f.yml", &PG_F.replace("{dep}", ""));
    p.file("app.t.yml", &table("app", escaped, "", ""));
    let planned = p.plan().unwrap();
    assert_eq!(planned.0, ["create_module app.f()", "create_table app.t"]);
    // `fmt` writes what the shorthand says, and then has nothing left to do.
    assert_ne!(p.said(&["fmt", "--check"]).0, Some(0));
    assert_eq!(p.said(&["fmt"]).0, Some(0));
    let formatted = std::fs::read_to_string(p.root.join("schema/app.t.yml")).unwrap();
    assert!(formatted.contains("depends_on: [app.f()]"), "{formatted}");
    let (code, text) = p.said(&["fmt", "--check"]);
    assert_eq!(code, Some(0), "{text}");
    assert_eq!(p.plan().unwrap(), planned);
    // Negative: text that is neither a shorthand nor structured is refused
    // naming the file, not read as something else.
    p.file(
        "app.t.yml",
        &table("app", "depends_on: [\"object::nonsense\"]\n", "", ""),
    );
    let (code, text) = p.said(&["validate"]);
    assert_ne!(code, Some(0), "{text}");
    assert!(text.contains("malformed structured reference"), "{text}");
}

#[test]
fn fmt_keeps_an_owner_map_and_is_idempotent_through_the_command() {
    let p = Project::new("postgres");
    p.file("app.f.yml", &PG_F.replace("{dep}", ""));
    p.file(
        "app.t.yml",
        &table(
            "app",
            "depends_on:\n  \"table::app.t\": [\"module::app.f()\"]\n  \"default::app.t.x\": [\"module::app.f()\"]\n",
            ", default: \"0\"",
            "",
        ),
    );
    assert_eq!(p.said(&["fmt"]).0, Some(0));
    let once = std::fs::read_to_string(p.root.join("schema/app.t.yml")).unwrap();
    assert!(once.contains("default::app.t.x"), "{once}");
    assert_eq!(p.said(&["fmt"]).0, Some(0));
    assert_eq!(
        std::fs::read_to_string(p.root.join("schema/app.t.yml")).unwrap(),
        once
    );
    // The formatted project plans what the original did.
    assert_eq!(p.plan().unwrap().1, 2);
}

// The review examples of #2423 and the implicit-regclass view (#2469, acceptance
// 1): each is planned from YAML on the route without a resolver, and one
// `depends_on:` line orders it. The examples are the findings of
// https://github.com/pongbiphang/pbps/pull/2423 that a text match misread.

/// A PostgreSQL project of these files, planned.
fn planned(files: &[(&str, String)]) -> Result<(Vec<String>, usize), String> {
    let p = Project::new("postgres");
    for (name, body) in files {
        p.file(name, body);
    }
    p.plan()
}

fn with_line(body: &str, line: &str) -> String {
    body.replace("{dep}", line)
}

fn without_line(body: &str) -> String {
    body.replace("{dep}", "")
}

/// The domain `app.a` and `app.b`, each with a text default that is the other's name
/// (review r4376835811).
const DOMAIN_A: &str = "domain: app.a\n{dep}type: text\ndefault: \"'b'\"\n";
const DOMAIN_B: &str = "domain: app.b\n{dep}type: text\ndefault: \"'a'\"\n";
/// A default that names a view only through a regclass literal: the scan does
/// not read it as a dependency, and the engine resolves it when it creates the
/// table.
const REGCLASS_TABLE: &str = "table: app.t\n{dep}columns:\n  id: {type: int, nullable: false}\n  x: {type: int, default: \"('app.w'::regclass)::oid::integer\"}\n";
const REGCLASS_VIEW: &str = "view: app.w\n{dep}definition: SELECT 1 AS n\n";

#[test]
fn text_names_that_are_data_or_literals_are_ordered_by_one_depends_on_line() {
    let a = |line: &str| ("app.a.yml", with_line(DOMAIN_A, line));
    let b = |line: &str| ("app.b.yml", with_line(DOMAIN_B, line));
    let t = |line: &str| ("app.t.yml", with_line(REGCLASS_TABLE, line));
    let w = |line: &str| ("app.w.yml", with_line(REGCLASS_VIEW, line));
    let both = &["create_type app.a", "create_type app.b"];
    let w_first = &["create_module app.w", "create_table app.t"];
    let cases = [
        // Two domains whose defaults match each other's names: the strings are
        // data, so nothing orders them, and the line chooses.
        (
            "domain defaults that are each other's names",
            vec![a(""), b("")],
            both,
            vec![a("depends_on: [\"type::app.b\"]\n"), b("")],
            &["create_type app.b", "create_type app.a"][..],
        ),
        // A default that names a view only in a regclass literal: the table
        // would be created before the view the engine needs, and a line on the
        // table puts the view first.
        (
            "a table default that names a view by regclass",
            vec![t(""), w("")],
            &["create_table app.t", "create_module app.w"],
            vec![t("depends_on: [\"module::app.w\"]\n"), w("")],
            w_first,
        ),
        // The same, with the line on the default part alone: the table stays
        // first and its default follows the view.
        (
            "the default part of that table",
            vec![t(""), w("")],
            &["create_table app.t", "create_module app.w"],
            vec![
                t("depends_on:\n  \"default::app.t.x\": [\"module::app.w\"]\n"),
                w(""),
            ],
            &[
                "create_table app.t",
                "create_module app.w",
                "alter_column_default",
            ],
        ),
    ];
    for (name, plain_files, plain, annotated_files, then) in cases {
        let (steps, edges) = planned(&plain_files).unwrap_or_else(|why| panic!("{name}: {why}"));
        assert_eq!(steps, plain, "{name} without the line");
        assert_eq!(edges, 0, "{name} without the line");
        let (steps, edges) =
            planned(&annotated_files).unwrap_or_else(|why| panic!("{name}: {why}"));
        assert_eq!(steps, then, "{name} with the line");
        assert_eq!(edges, 1, "{name} with the line");
    }
}

#[test]
fn a_line_on_the_wrong_owner_leaves_the_order_that_text_gave() {
    let a = |line: &str| ("app.a.yml", with_line(DOMAIN_A, line));
    let b = |line: &str| ("app.b.yml", with_line(DOMAIN_B, line));
    let t = ("app.t.yml", without_line(REGCLASS_TABLE));
    let w = |line: &str| ("app.w.yml", with_line(REGCLASS_VIEW, line));
    // `app.b` following `app.a` is the order the plain plan already has.
    let (steps, edges) = planned(&[a(""), b("depends_on: [\"type::app.a\"]\n")]).unwrap();
    assert_eq!(steps, ["create_type app.a", "create_type app.b"]);
    assert_eq!(edges, 1);
    // The view following the table does not move the table's default behind
    // the view: the table is still created first, and the line is not the fix.
    let (steps, edges) = planned(&[t, w("depends_on: [\"table::app.t\"]\n")]).unwrap();
    assert_eq!(steps, ["create_table app.t", "create_module app.w"]);
    assert_eq!(edges, 1);
}

#[test]
fn a_review_example_the_scan_already_orders_keeps_that_order_with_one_line() {
    const F: &str = "function: app.f(integer)\n{dep}definition: |-\n  (id integer) RETURNS integer LANGUAGE sql IMMUTABLE RETURN id\n";
    // A generated column that calls `app.f` and is declared before its input,
    // and an index named like the routine's parameter (review r4377973392).
    const HELD: &str = "table: app.t\n{dep}columns:\n  id: {type: int, nullable: false}\n  x: {type: text}\n  g: {type: int, generated: {expression: \"app.f(z)\", stored: true}}\n  z: {type: int}\nindexes:\n  id: {columns: [x]}\n";
    // A view named like the built-in type of a column it reads (r4380096888).
    const READER: &str = "view: app.text\n{dep}definition: SELECT x FROM app.t\n";
    const PLAIN_TABLE: &str = "table: app.t\n{dep}columns:\n  x: {type: text}\n";
    // `app.value` is a domain over `app.positive`, whose check holds `VALUE`
    // (r4377973376).
    const POSITIVE: &str = "domain: app.positive\n{dep}type: int\nchecks:\n  c: \"VALUE > 0\"\n";
    const VALUE: &str = "domain: app.value\n{dep}type: app.positive\n";
    // A view named in a regclass literal of a view over the table (r4383911676).
    const SIZE: &str = "view: app.v\n{dep}definition: SELECT pg_relation_size('app.t') AS n\n";
    // Two routines that call each other late, and a routine that declares a
    // view's row type (the draft rounds 1 and 2).
    const CALLS_G: &str = "function: app.f()\n{dep}definition: |-\n  () RETURNS integer LANGUAGE plpgsql AS $$ BEGIN RETURN app.g(); END $$\n";
    const CALLS_F: &str = "function: app.g()\n{dep}definition: |-\n  () RETURNS integer LANGUAGE plpgsql AS $$ BEGIN RETURN app.f(); END $$\n";
    const VIEW: &str = "view: app.v\n{dep}definition: SELECT 1 AS n\n";
    const ROWTYPE: &str = "function: app.r()\n{dep}definition: |-\n  () RETURNS integer LANGUAGE plpgsql AS $$ DECLARE x app.v%ROWTYPE; BEGIN RETURN 1; END $$\n";
    type Scene = (
        &'static str,
        Vec<(&'static str, &'static str)>,
        usize,
        &'static str,
        &'static [&'static str],
    );
    // (name, files, index of the file that takes the line, the line, the order)
    let scenes: Vec<Scene> = vec![
        (
            "a held table before the function it calls, and an index named like its parameter",
            vec![("f.yml", F), ("t.yml", HELD)],
            1,
            "depends_on: [\"module::app.f(integer)\"]\n",
            &["create_module app.f(integer)", "create_table app.t"],
        ),
        (
            "a view named like a built-in type, after the table it reads",
            vec![("t.yml", PLAIN_TABLE), ("v.yml", READER)],
            1,
            "depends_on: [\"table::app.t\"]\n",
            &["create_table app.t", "create_module app.text"],
        ),
        (
            "a domain named VALUE over a domain whose check holds VALUE",
            vec![("p.yml", POSITIVE), ("v.yml", VALUE)],
            1,
            "depends_on: [\"type::app.positive\"]\n",
            &["create_type app.positive", "create_type app.value"],
        ),
        (
            "a view over a table named in an implicit regclass argument",
            vec![
                (
                    "t.yml",
                    "table: app.t\n{dep}columns:\n  id: {type: int, nullable: false}\n",
                ),
                ("v.yml", SIZE),
            ],
            1,
            "depends_on: [\"table::app.t\"]\n",
            &["create_table app.t", "create_module app.v"],
        ),
        (
            "two routines that call each other late",
            vec![("f.yml", CALLS_G), ("g.yml", CALLS_F)],
            1,
            "depends_on: [\"module::app.f()\"]\n",
            &["create_module app.f()", "create_module app.g()"],
        ),
        (
            "a routine that declares the row type of a view",
            vec![("v.yml", VIEW), ("r.yml", ROWTYPE)],
            1,
            "depends_on: [\"module::app.v\"]\n",
            &["create_module app.v", "create_module app.r()"],
        ),
    ];
    for (name, files, owner, line, order) in scenes {
        let plain: Vec<_> = files
            .iter()
            .map(|(file, body)| (*file, without_line(body)))
            .collect();
        let (steps, edges) = planned(&plain).unwrap_or_else(|why| panic!("{name}: {why}"));
        assert_eq!(steps, order, "{name} without the line");
        assert_eq!(edges, 0, "{name} without the line");
        let annotated: Vec<_> = files
            .iter()
            .enumerate()
            .map(|(i, (file, body))| {
                (
                    *file,
                    if i == owner {
                        with_line(body, line)
                    } else {
                        without_line(body)
                    },
                )
            })
            .collect();
        let (steps, edges) = planned(&annotated).unwrap_or_else(|why| panic!("{name}: {why}"));
        assert_eq!(steps, order, "{name} with the line");
        assert_eq!(edges, 1, "{name} with the line");
    }
}

// ---------------------------------------------------------------------------
// A line that contradicts a creation fact is refused (#2469 F, DEC-2469.10).
//
// A creation fact is something a step needs to exist first: a table's type, a
// partition's parent, a trigger's table. The differ holds most of them in its
// sort class and rank, where no line can make a cycle, so a line that reversed
// one used to write a plan the engine would refuse. They are edges now, beside
// the lines, and the cycle is refused naming the line.

use pbps_model::object_ref::{ObjectRef, Root, Selector};
use pbps_model::operation::PartKey;

/// What the suite holds for a kind of provider a line can name.
enum Fact {
    /// These cases reverse a fact of the kind.
    Cases(&'static [&'static str]),
    /// No line can, or the fact is not an edge, for this reason.
    NoCase(&'static str),
}

/// Every kind of reference a line can name as the provider, each with the
/// cases that contradict a creation fact through it, or the reason it has
/// none. An exhaustive match with no wildcard: a new kind of reference does
/// not compile until it is placed here.
fn fact_of(provider: &ObjectRef) -> Fact {
    match &provider.part {
        Selector::Whole => match &provider.root {
            Root::Table(_) => Fact::Cases(&[
                "enum-under-column",
                "domain-under-column",
                "partition-under-parent",
            ]),
            Root::Column(_) => Fact::Cases(&["own-column-pg", "own-column-ms"]),
            Root::Type(_) => Fact::Cases(&["domain-over-domain", "enum-under-domain"]),
            // A view over a view and a table under the view that reads it are
            // text-derived: a line may displace them (decision 8.3 = A).
            Root::Module(_) => Fact::Cases(&["trigger-before-table-pg", "trigger-before-table-ms"]),
            Root::Role(_) => {
                Fact::NoCase("a PostgreSQL role is provisioned outside the plan: no step to order")
            }
        },
        Selector::Default => Fact::Cases(&["own-default-pg", "own-default-ms"]),
        Selector::Generation => Fact::Cases(&["own-generation-pg"]),
        Selector::Sequence => Fact::Cases(&["own-sequence-pg"]),
        Selector::NotNull => Fact::Cases(&["own-not-null-pg", "own-not-null-ms"]),
        Selector::Part(part) => match part {
            PartKey::PrimaryKey => Fact::Cases(&["own-primary-key-pg", "own-primary-key-ms"]),
            PartKey::Unique(_) => Fact::Cases(&["own-unique-pg", "own-unique-ms"]),
            PartKey::ForeignKey(_) => Fact::NoCase(
                "a foreign key is its own step after both tables, so no line reverses it (2e)",
            ),
            PartKey::Check(_) => Fact::Cases(&["own-check-pg", "own-check-ms"]),
            PartKey::Exclusion(_) => Fact::Cases(&["exclusion-before-table-pg"]),
            PartKey::Policy(_) => Fact::Cases(&["own-policy-pg"]),
            PartKey::RowSecurity => Fact::Cases(&["own-row-security-pg"]),
            PartKey::Index(_) => {
                Fact::Cases(&["own-index-pg", "own-index-ms", "own-view-index-ms"])
            }
            PartKey::Computed(_) => Fact::Cases(&["own-computed-ms"]),
        },
    }
}

/// One reference per arm of [`fact_of`], so each arm is read.
const KINDS: &[&str] = &[
    "table::app.t",
    "column::app.t.x",
    "type::app.d",
    "module::app.v",
    "role::app.r",
    "default::app.t.x",
    "generation::app.t.x",
    "sequence::app.t.id",
    "not_null::app.t.x",
    "primary_key::app.t",
    "unique::app.t.u",
    "foreign_key::app.t.fk",
    "check::app.t.c",
    "exclusion::app.t.e",
    "policy::app.t.p",
    "row_security::app.t",
    "index::app.t.ix",
    "object::{\"root\":{\"table\":\"dbo.t\"},\"part\":{\"computed\":\"x\"}}",
];

/// A project whose one `depends_on:` line reverses a creation fact.
struct Contradiction {
    name: &'static str,
    dialect: &'static str,
    /// The files, with `{dep}` where the owner's file takes the line.
    files: Vec<(&'static str, String)>,
    owner_file: &'static str,
    /// The line as YAML, and as the refusal spells it.
    yaml: String,
    owner: &'static str,
    provider: &'static str,
    /// The members the refusal names, where a case pins them.
    members: Option<&'static str>,
}

fn tbl(schema: &str, cols: &str, rest: &str) -> String {
    format!(
        "table: {schema}.t\n{{dep}}columns:\n  id: {{type: int, nullable: false}}\n{cols}{rest}"
    )
}

/// A table whose own part `provider` the line puts before the table.
fn own(
    name: &'static str,
    dialect: &'static str,
    schema: &str,
    cols: &str,
    rest: &str,
    provider: &'static str,
) -> Contradiction {
    Contradiction {
        name,
        dialect,
        files: vec![("t.yml", tbl(schema, cols, rest))],
        owner_file: "t.yml",
        yaml: format!("depends_on:\n  \"table::{schema}.t\": [\"{provider}\"]\n"),
        owner: if schema == "app" {
            "table::app.t"
        } else {
            "table::dbo.t"
        },
        provider,
        members: None,
    }
}

fn contradictions() -> Vec<Contradiction> {
    const X: &str = "  x: {type: int}\n";
    let mut cases = vec![
        // Typed facts the differ holds by class and rank (2b).
        Contradiction {
            name: "domain-over-domain",
            dialect: "postgres",
            files: vec![
                (
                    "p.yml",
                    "domain: app.positive\n{dep}type: int\nchecks:\n  c: \"VALUE > 0\"\n".into(),
                ),
                ("v.yml", "domain: app.value\ntype: app.positive\n".into()),
            ],
            owner_file: "p.yml",
            yaml: "depends_on: [\"type::app.value\"]\n".into(),
            owner: "type::app.positive",
            provider: "type::app.value",
            members: None,
        },
        Contradiction {
            name: "enum-under-domain",
            dialect: "postgres",
            files: vec![
                ("d.yml", "domain: app.d\ntype: app.mood\n".into()),
                ("m.yml", "enum: app.mood\n{dep}values: [a, b]\n".into()),
            ],
            owner_file: "m.yml",
            yaml: "depends_on: [\"type::app.d\"]\n".into(),
            owner: "type::app.mood",
            provider: "type::app.d",
            members: None,
        },
        Contradiction {
            name: "enum-under-column",
            dialect: "postgres",
            files: vec![
                ("m.yml", "enum: app.mood\n{dep}values: [a, b]\n".into()),
                ("t.yml", "table: app.t\ncolumns:\n  id: {type: int, nullable: false}\n  m: {type: app.mood}\n".into()),
            ],
            owner_file: "m.yml",
            yaml: "depends_on: [\"table::app.t\"]\n".into(),
            owner: "type::app.mood",
            provider: "table::app.t",
            members: None,
        },
        Contradiction {
            name: "domain-under-column",
            dialect: "postgres",
            files: vec![
                ("d.yml", "domain: app.d\n{dep}type: int\n".into()),
                ("t.yml", "table: app.t\ncolumns:\n  id: {type: int, nullable: false}\n  m: {type: app.d}\n".into()),
            ],
            owner_file: "d.yml",
            yaml: "depends_on: [\"table::app.t\"]\n".into(),
            owner: "type::app.d",
            provider: "table::app.t",
            members: None,
        },
        // Typed facts the check did not even report (2c).
        own(
            "exclusion-before-table-pg",
            "postgres",
            "app",
            X,
            "exclude:\n  e:\n    method: btree\n    keys:\n      - {column: x, operator: \"=\"}\n",
            "exclusion::app.t.e",
        ),
        Contradiction {
            name: "partition-under-parent",
            dialect: "postgres",
            files: vec![(
                "p.yml",
                "table: app.p\n{dep}columns:\n  id: {type: int, nullable: false}\n  ts: {type: int, nullable: false}\npartition_by: [ts]\npartitions:\n  c1: {from: [\"0\"], to: [\"10\"]}\n".into(),
            )],
            owner_file: "p.yml",
            yaml: "depends_on:\n  \"table::app.p\": [\"table::app.c1\"]\n".into(),
            owner: "table::app.p",
            provider: "table::app.c1",
            members: None,
        },
        Contradiction {
            name: "trigger-before-table-pg",
            dialect: "postgres",
            files: vec![
                ("t.yml", tbl("app", X, "")),
                (
                    "tf.yml",
                    "function: app.tf()\ndefinition: |-\n  () RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RETURN NEW; END $$\n".into(),
                ),
                (
                    "trg.yml",
                    "trigger: app.trg\non: app.t\ndefinition: |-\n  BEFORE INSERT ON app.t FOR EACH ROW EXECUTE FUNCTION app.tf()\n".into(),
                ),
            ],
            owner_file: "t.yml",
            yaml: "depends_on: [\"module::app.t.trg\"]\n".into(),
            owner: "table::app.t",
            provider: "module::app.t.trg",
            members: Some("`app.t`, `app.t.trg`"),
        },
        Contradiction {
            name: "trigger-before-table-ms",
            dialect: "mssql",
            files: vec![
                ("t.yml", tbl("dbo", X, "")),
                (
                    "trg.yml",
                    "trigger: dbo.trg\non: dbo.t\ndefinition: |-\n  AFTER INSERT AS SELECT 1\n".into(),
                ),
            ],
            owner_file: "t.yml",
            yaml: "depends_on: [\"module::dbo.t.trg\"]\n".into(),
            owner: "table::dbo.t",
            provider: "module::dbo.t.trg",
            members: Some("`dbo.t`, `dbo.t.trg`"),
        },
        // A table's own parts (2a), which the CREATE TABLE group refused
        // already and which now name the line too.
        own("own-index-pg", "postgres", "app", X, "indexes:\n  ix: {columns: [x]}\n", "index::app.t.ix"),
        own("own-check-pg", "postgres", "app", X, "checks:\n  c: \"x > 0\"\n", "check::app.t.c"),
        own("own-unique-pg", "postgres", "app", X, "unique:\n  u: {columns: [x]}\n", "unique::app.t.u"),
        own("own-primary-key-pg", "postgres", "app", X, "primary_key: [id]\n", "primary_key::app.t"),
        own("own-default-pg", "postgres", "app", "  x: {type: int, default: \"0\"}\n", "", "default::app.t.x"),
        own("own-not-null-pg", "postgres", "app", "  x: {type: int, nullable: false}\n", "", "not_null::app.t.x"),
        own("own-generation-pg", "postgres", "app", "  x: {type: int, generated: {expression: \"id + 1\", stored: true}}\n", "", "generation::app.t.x"),
        own("own-column-pg", "postgres", "app", X, "", "column::app.t.x"),
        own("own-policy-pg", "postgres", "app", X, "row_security: {enabled: true}\nrow_policies:\n  p: {using: \"true\"}\n", "policy::app.t.p"),
        own("own-row-security-pg", "postgres", "app", X, "row_security: {enabled: true}\n", "row_security::app.t"),
        own("own-index-ms", "mssql", "dbo", X, "indexes:\n  ix: {columns: [x]}\n", "index::dbo.t.ix"),
        own("own-check-ms", "mssql", "dbo", X, "checks:\n  c: \"x > 0\"\n", "check::dbo.t.c"),
        own("own-unique-ms", "mssql", "dbo", X, "unique:\n  u: {columns: [x]}\n", "unique::dbo.t.u"),
        own("own-primary-key-ms", "mssql", "dbo", X, "primary_key: [id]\n", "primary_key::dbo.t"),
        own("own-default-ms", "mssql", "dbo", "  x: {type: int, default: \"0\"}\n", "", "default::dbo.t.x"),
        own("own-not-null-ms", "mssql", "dbo", "  x: {type: int, nullable: false}\n", "", "not_null::dbo.t.x"),
        own("own-column-ms", "mssql", "dbo", X, "", "column::dbo.t.x"),
    ];
    // A computed column has no short prefix, only the structured spelling.
    cases.push(Contradiction {
        name: "own-computed-ms",
        dialect: "mssql",
        files: vec![(
            "t.yml",
            "table: dbo.t\n{dep}columns:\n  id: {type: int, nullable: false}\ncomputed:\n  x: {expression: \"id + 1\", persisted: true}\n".into(),
        )],
        owner_file: "t.yml",
        yaml: "depends_on:\n  \"table::dbo.t\": ['object::{\"root\":{\"table\":\"dbo.t\"},\"part\":{\"computed\":\"x\"}}']\n".into(),
        owner: "table::dbo.t",
        provider: "object::{\"root\":{\"table\":\"dbo.t\"},\"part\":{\"computed\":\"x\"}}",
        members: None,
    });
    cases.push(Contradiction {
        name: "own-sequence-pg",
        dialect: "postgres",
        files: vec![(
            "t.yml",
            "table: app.t\n{dep}columns:\n  id: {type: serial}\n".into(),
        )],
        owner_file: "t.yml",
        yaml: "depends_on:\n  \"table::app.t\": [\"sequence::app.t.id\"]\n".into(),
        owner: "table::app.t",
        provider: "sequence::app.t.id",
        members: None,
    });
    cases.push(Contradiction {
        name: "own-view-index-ms",
        dialect: "mssql",
        files: vec![
            ("t.yml", "table: dbo.t\ncolumns:\n  id: {type: int, nullable: false}\n".into()),
            (
                "v.yml",
                "view: dbo.v\nschemabinding: true\n{dep}indexes:\n  ix: {unique: true, columns: [id]}\nclustered: ix\ndefinition: |-\n  SELECT id FROM dbo.t\n".into(),
            ),
        ],
        owner_file: "v.yml",
        yaml: "depends_on:\n  \"module::dbo.v\": [\"index::module::dbo.v.ix\"]\n".into(),
        owner: "module::dbo.v",
        provider: "index::module::dbo.v.ix",
        members: None,
    });
    cases
}

impl Contradiction {
    fn at(&self, annotated: bool) -> Project {
        let p = Project::new(self.dialect);
        for (name, body) in &self.files {
            let line = if annotated && *name == self.owner_file {
                self.yaml.as_str()
            } else {
                ""
            };
            p.file(name, &body.replace("{dep}", line));
        }
        p
    }

    /// The line as the refusal names it, spelt as the projection error does.
    fn line(&self) -> String {
        format!("annotation `{}` depends on `{}`", self.owner, self.provider)
    }
}

#[test]
fn a_line_that_contradicts_a_creation_fact_is_refused_naming_the_line() {
    let failures: Vec<String> = contradictions()
        .iter()
        .filter_map(|case| {
            let name = case.name;
            // Negative: the project without the line plans.
            if let Err(why) = case.at(false).plan() {
                return Some(format!("{name}: refused without the line: {why}"));
            }
            let p = case.at(true);
            let out = p.root.join("plan.json");
            let (code, text) = p.said(&["plan", "--out", out.to_str().unwrap()]);
            // A refusal, not a panic of a debug build (101) and not a plan (0).
            if code != Some(1) {
                return Some(format!("{name}: exit {code:?}, not a refusal: {text}"));
            }
            if !text.contains("these steps wait on one another")
                && !text.contains("cannot project annotation")
            {
                return Some(format!("{name}: refused, but not as a cycle: {text}"));
            }
            // The cycle names its members: a step printed as `#<n>` is not a
            // name (acceptance 3 of #2469).
            if let Some(members) = text
                .split("wait on one another: ")
                .nth(1)
                .and_then(|rest| rest.split("; because").next())
                && members.split(", ").any(|m| {
                    m.strip_prefix('#')
                        .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
                })
            {
                return Some(format!("{name}: a member is a placeholder: {members}"));
            }
            if let Some(members) = case.members
                && !text.contains(&format!("wait on one another: {members}; because"))
            {
                return Some(format!("{name}: members are not `{members}`: {text}"));
            }
            (!text.contains(&case.line())).then(|| {
                format!(
                    "{name}: the refusal does not name the line `{}`: {text}",
                    case.line()
                )
            })
        })
        .collect();
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn every_kind_of_reference_has_a_case_or_a_reason() {
    let cases = contradictions();
    let known: Vec<&str> = cases.iter().map(|c| c.name).collect();
    let mut named = std::collections::BTreeSet::new();
    for kind in KINDS {
        let reference: ObjectRef = kind.parse().unwrap_or_else(|e| panic!("{kind}: {e}"));
        match fact_of(&reference) {
            Fact::Cases(names) => {
                assert!(!names.is_empty(), "{kind} names no case");
                for name in names {
                    assert!(known.contains(name), "{kind}: no case `{name}`");
                    named.insert(*name);
                }
            }
            Fact::NoCase(reason) => assert!(!reason.is_empty(), "{kind} has no reason"),
        }
    }
    // Each case answers to the kind of its provider, and each kind a case
    // names is read above: no case hangs outside the list.
    for case in &cases {
        let provider: ObjectRef = case.provider.parse().unwrap();
        let Fact::Cases(names) = fact_of(&provider) else {
            panic!("{}: its provider's kind has no case", case.name);
        };
        assert!(
            names.contains(&case.name),
            "{}: not listed for `{}`",
            case.name,
            case.provider
        );
        assert!(
            named.contains(case.name),
            "{}: no kind in KINDS reaches it",
            case.name
        );
    }
}

#[test]
fn a_line_that_agrees_with_a_creation_fact_is_accepted_and_orders_as_before() {
    // The trigger follows its table in the plan; a line that says so is
    // redundant, and the plan is the one without it.
    let trigger = &contradictions()
        .into_iter()
        .find(|c| c.name == "trigger-before-table-pg")
        .unwrap();
    let plain = trigger.at(false).plan().unwrap().0;
    let agreeing = trigger.at(false);
    agreeing.file(
        "trg.yml",
        "trigger: app.trg\non: app.t\ndepends_on: [\"table::app.t\"]\ndefinition: |-\n  BEFORE INSERT ON app.t FOR EACH ROW EXECUTE FUNCTION app.tf()\n",
    );
    let (steps, edges) = agreeing.plan().unwrap();
    assert_eq!(steps, plain);
    assert_eq!(edges, 1);
    // The type likewise follows what it is over.
    let types = &contradictions()
        .into_iter()
        .find(|c| c.name == "domain-over-domain")
        .unwrap();
    let plain = types.at(false).plan().unwrap().0;
    let agreeing = types.at(false);
    agreeing.file(
        "v.yml",
        "domain: app.value\ndepends_on: [\"type::app.positive\"]\ntype: app.positive\n",
    );
    assert_eq!(agreeing.plan().unwrap().0, plain);
}

#[test]
fn a_line_that_reverses_a_text_derived_fact_is_followed_on_purpose() {
    // A view over a view is read from the text, which a line may displace
    // (decision 8.3 = A); the creation facts above are not.
    let p = Project::new("postgres");
    p.file("w.yml", "view: app.w\ndefinition: SELECT 1 AS n\n");
    p.file("v.yml", "view: app.v\ndefinition: SELECT n FROM app.w\n");
    let plain = p.plan().unwrap().0;
    assert_eq!(plain, ["create_module app.w", "create_module app.v"]);
    p.file(
        "w.yml",
        "view: app.w\ndepends_on: [\"app.v\"]\ndefinition: SELECT 1 AS n\n",
    );
    let (steps, _) = p.plan().unwrap();
    assert_eq!(steps, ["create_module app.v", "create_module app.w"]);
}
