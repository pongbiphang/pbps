use super::*;

/// A plan the resolver answers, published through the binary (#1516). The
/// target holds one routine `app.f(numeric)` that a default, a CHECK, an
/// index predicate, a view and a routine header call as `app.f(1)`; the
/// revision declares `app.f(integer)`, which captures every one of those
/// calls. The scratch is a confined login in its own database on the
/// target's server: the one layout both pinned servers support.
pub fn published(server: &str) {
    let own = OwnDatabase::new(server, "publish1516");
    let connection = own.connection();
    on_server(connection, "CREATE SCHEMA app");
    let scratch = Confined::new(server);
    let d = Demo::new("publish1516");
    declare(&d);
    successful(run(&d, &["plan"], None));
    d.commit();
    successful(run(&d, &["bootstrap", "--db", connection], None));
    std::fs::write(
        d.dir.join("schema/app.f%28integer%29.function.yml"),
        "function: app.f(integer)\npublic_execute: true\n\ndefinition: |-\n  \
         (integer) RETURNS numeric LANGUAGE sql IMMUTABLE RETURN $1\n",
    )
    .unwrap();
    successful(run(&d, &["plan"], None));
    d.commit();
    configure(&d);
    successful(run(
        &d,
        &[
            "key",
            "generate",
            "--out",
            d.dir.join("key").to_str().unwrap(),
        ],
        None,
    ));
    let targets = Targets {
        target: connection,
        scratch: &scratch.connection,
    };
    let artifact = d.dir.join("plan.json");
    let sql = d.dir.join("plan.sql");

    // A grant the rebuilt view carries, with an option the declarations
    // cannot say: the ordinary rebuild check refuses the resolver's plan,
    // before anything is written.
    let grantee = format!("pbps_cli_g1516_{}", std::process::id());
    on_server(
        connection,
        &format!(
            "DROP ROLE IF EXISTS {grantee}; CREATE ROLE {grantee} NOLOGIN; \
             GRANT SELECT ON app.v TO {grantee} WITH GRANT OPTION"
        ),
    );
    write_prior(&artifact, &sql);
    let refused = run(&d, &plan_args(&artifact, &sql), Some(&targets));
    on_server(
        connection,
        &format!("REVOKE ALL ON app.v FROM {grantee}; DROP ROLE {grantee}"),
    );
    assert_ne!(
        code(&refused),
        0,
        "{}{}",
        stdout(&refused),
        stderr(&refused)
    );
    let said = format!("{}{}", stdout(&refused), stderr(&refused));
    assert!(
        said.contains("GRANT OPTION"),
        "the rebuild check names the grant: {said}"
    );
    assert_prior(&artifact, &sql);

    // Without it, the plan is published.
    let output = successful(run(&d, &plan_args(&artifact, &sql), Some(&targets)));
    let value: serde_json::Value = serde_json::from_str(&stdout(&output)).unwrap();
    assert_eq!(
        value["data"]["resolver_assessment"]["need"], "resolved",
        "{value}"
    );
    let saved: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&artifact).unwrap()).unwrap();
    assert_eq!(saved["origin"], "resolved_database", "{saved}");
    let cluster = saved["analysis"]["evidence"]["qualification"]["target_cluster"]
        .as_str()
        .unwrap_or_else(|| panic!("the evidence seals the target's cluster: {saved}"));
    assert!(
        cluster.len() == 64 && cluster.bytes().all(|b| b.is_ascii_hexdigit()),
        "keyed, never the identifier in clear: {cluster}"
    );
    assert_exact_order(&std::fs::read_to_string(&sql).unwrap());

    // A hand-edited cluster identity is a plan other than the one whose
    // checksum was approved.
    let mut edited = saved.clone();
    edited["analysis"]["evidence"]["qualification"]["target_cluster"] =
        serde_json::Value::String("0".repeat(64));
    let tampered = d.dir.join("tampered.json");
    std::fs::write(&tampered, serde_json::to_string_pretty(&edited).unwrap()).unwrap();
    let checksum = |path: &std::path::Path| {
        let output = successful(run(
            &d,
            &["explain", "--plan", path.to_str().unwrap()],
            None,
        ));
        stdout(&output)
            .lines()
            .find_map(|line| {
                line.trim()
                    .strip_prefix("checksum")
                    .map(|c| c.trim().to_owned())
            })
            .unwrap_or_else(|| panic!("explain names the checksum: {}", stdout(&output)))
    };
    assert_ne!(checksum(&tampered), checksum(&artifact));

    // A `--sql` that cannot be written leaves no new `--out` behind.
    write_prior(&artifact, &sql);
    let unwritable = d.dir.join("absent-1516").join("plan.sql");
    let failed = run(&d, &plan_args(&artifact, &unwritable), Some(&targets));
    assert_ne!(code(&failed), 0, "{}{}", stdout(&failed), stderr(&failed));
    assert_prior(&artifact, &sql);
}

/// An ordinary connected check refuses a resolver's plan as it refuses an
/// ordinary one (#1516): a changed generation expression needs PostgreSQL
/// 17's `SET EXPRESSION`. On 16 nothing is written; on 18, the same
/// revision is published.
pub fn ordinary_checks(server: &str, refused: bool) {
    let own = OwnDatabase::new(server, "checks1516");
    let connection = own.connection();
    on_server(connection, "CREATE SCHEMA app");
    let scratch = Confined::new(server);
    let d = Demo::new("checks1516");
    declare(&d);
    let generated = "table: app.g\ncolumns:\n  id: {type: integer, nullable: false}\n  \
                     twice: {type: integer, generated: {expression: 'id * 2', stored: true}}\n\
                     primary_key: {name: pk_g, columns: [id]}\n";
    std::fs::write(d.dir.join("schema/app.g.yml"), generated).unwrap();
    successful(run(&d, &["plan"], None));
    d.commit();
    successful(run(&d, &["bootstrap", "--db", connection], None));
    std::fs::write(
        d.dir.join("schema/app.f%28integer%29.function.yml"),
        "function: app.f(integer)\npublic_execute: true\n\ndefinition: |-\n  \
         (integer) RETURNS numeric LANGUAGE sql IMMUTABLE RETURN $1\n",
    )
    .unwrap();
    std::fs::write(
        d.dir.join("schema/app.g.yml"),
        generated.replace("id * 2", "id * 3"),
    )
    .unwrap();
    successful(run(&d, &["plan"], None));
    d.commit();
    configure(&d);
    successful(run(
        &d,
        &[
            "key",
            "generate",
            "--out",
            d.dir.join("key").to_str().unwrap(),
        ],
        None,
    ));
    let targets = Targets {
        target: connection,
        scratch: &scratch.connection,
    };
    let artifact = d.dir.join("plan.json");
    let sql = d.dir.join("plan.sql");
    write_prior(&artifact, &sql);
    let output = run(&d, &plan_args(&artifact, &sql), Some(&targets));
    if refused {
        assert_ne!(code(&output), 0, "{}{}", stdout(&output), stderr(&output));
        let said = format!("{}{}", stdout(&output), stderr(&output));
        assert!(said.contains("SET EXPRESSION"), "{said}");
        assert_prior(&artifact, &sql);
    } else {
        let output = successful(output);
        let value: serde_json::Value = serde_json::from_str(&stdout(&output)).unwrap();
        assert_eq!(
            value["data"]["resolver_assessment"]["need"], "resolved",
            "{value}"
        );
        assert!(
            std::fs::read_to_string(&sql)
                .unwrap()
                .contains("SET EXPRESSION"),
            "the expression change is published with the resolver's answer"
        );
    }
}

/// The declarations every case starts from: `app.f(numeric)`, the
/// expression surfaces the capture covers that call it, and a view `w`
/// over the routine whose header does. A trigger's `WHEN`
/// is not one yet: the capture refuses it by name.
fn declare(d: &Demo) {
    let files = [
        (
            "app.f%28numeric%29.function.yml",
            "function: app.f(numeric)\npublic_execute: true\n\ndefinition: |-\n  \
             (numeric) RETURNS numeric LANGUAGE sql IMMUTABLE RETURN $1\n",
        ),
        (
            "app.t.yml",
            "table: app.t\ncolumns:\n  id: {type: integer, nullable: false}\n  \
             c: {type: numeric, default: app.f(1)}\n\
             primary_key: {name: pk_t, columns: [id]}\n\
             checks:\n  ck: c > app.f(1)\n\
             indexes:\n  ix:\n    columns: [c]\n    where: c > app.f(1)\n",
        ),
        (
            "app.v.view.yml",
            "view: app.v\ndefinition: SELECT app.f(1) AS x\n",
        ),
        (
            "app.w.view.yml",
            "view: app.w\ndefinition: SELECT app.h(2) AS y\n",
        ),
        (
            "app.h%28numeric%29.function.yml",
            "function: app.h(numeric)\npublic_execute: true\n\ndefinition: |-\n  \
             (x numeric DEFAULT app.f(1)) RETURNS numeric LANGUAGE sql IMMUTABLE RETURN x\n",
        ),
    ];
    for (name, body) in files {
        std::fs::write(d.dir.join("schema").join(name), body).unwrap();
    }
}

/// Each surface the arriving overload captures is created again after it,
/// so the engine binds it there, and the view over the rebuilt routine is
/// dropped before it and created after: the statements in the one order the
/// published SQL has. Each statement is scoped to its schema's write path
/// on its own.
fn assert_exact_order(script: &str) {
    let statements: Vec<String> = script
        .split("SET search_path = '';")
        .filter_map(|block| block.split_once("SET search_path = \"app\", \"pg_temp\";"))
        .map(|(_, statement)| {
            let words: Vec<&str> = statement.split_whitespace().collect();
            words.join(" ").trim_end_matches(';').trim_end().to_owned()
        })
        .collect();
    let expected = [
        r#"CREATE FUNCTION "app"."f" (integer) RETURNS numeric LANGUAGE sql IMMUTABLE RETURN $1"#,
        r#"GRANT EXECUTE ON ROUTINE "app"."f"(integer) TO PUBLIC"#,
        r#"DROP VIEW "app"."v""#,
        r#"CREATE VIEW "app"."v" AS SELECT app.f(1) AS x"#,
        r#"DROP VIEW "app"."w""#,
        r#"DROP FUNCTION "app"."h"(numeric)"#,
        r#"CREATE FUNCTION "app"."h" (x numeric DEFAULT app.f(1)) RETURNS numeric LANGUAGE sql IMMUTABLE RETURN x"#,
        r#"GRANT EXECUTE ON ROUTINE "app"."h"(numeric) TO PUBLIC"#,
        r#"CREATE VIEW "app"."w" AS SELECT app.h(2) AS y"#,
        r#"ALTER TABLE "app"."t" DROP CONSTRAINT "ck""#,
        r#"DROP INDEX "app"."ix""#,
        r#"ALTER TABLE "app"."t" ALTER COLUMN "c" DROP DEFAULT"#,
        r#"ALTER TABLE "app"."t" ALTER COLUMN "c" SET DEFAULT app.f(1)"#,
        r#"ALTER TABLE "app"."t" ADD CONSTRAINT "ck" CHECK (c > app.f(1) )"#,
        r#"CREATE INDEX "ix" ON "app"."t" ("c" ASC) WHERE (c > app.f(1) )"#,
    ];
    assert_eq!(statements, expected, "{script}");
}

fn configure(d: &Demo) {
    std::fs::write(
        d.dir.join("pbps.yml"),
        "dialect: postgres\nresolvers:\n  scratch:\n    kind: server\n    \
         url_env: PBPS_SCRATCH_1516\nenvironments:\n  prod:\n    url_env: PBPS_TARGET_1516\n    \
         resolve_with: scratch\n    fingerprint_key_file: key\n",
    )
    .unwrap();
}

struct Targets<'a> {
    target: &'a str,
    scratch: &'a str,
}

fn plan_args<'a>(artifact: &'a std::path::Path, sql: &'a std::path::Path) -> Vec<&'a str> {
    vec![
        "plan",
        "--env",
        "prod",
        "--out",
        artifact.to_str().unwrap(),
        "--sql",
        sql.to_str().unwrap(),
        "--format",
        "json",
    ]
}

fn write_prior(artifact: &std::path::Path, sql: &std::path::Path) {
    std::fs::write(artifact, "prior plan 1516").unwrap();
    std::fs::write(sql, "prior sql 1516").unwrap();
}

fn assert_prior(artifact: &std::path::Path, sql: &std::path::Path) {
    assert_eq!(std::fs::read(artifact).unwrap(), b"prior plan 1516");
    assert_eq!(std::fs::read(sql).unwrap(), b"prior sql 1516");
    let left: Vec<_> = std::fs::read_dir(artifact.parent().unwrap())
        .unwrap()
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.ends_with(".tmp"))
        .collect();
    assert!(left.is_empty(), "no staged file is left behind: {left:?}");
}

fn run(d: &Demo, args: &[&str], targets: Option<&Targets<'_>>) -> Output {
    let mut cmd = Command::new(BIN);
    cmd.arg("--project")
        .arg(&d.dir)
        .args(args)
        .env_remove("PBPS_TARGET_1516")
        .env_remove("PBPS_SCRATCH_1516");
    if let Some(targets) = targets {
        cmd.env("PBPS_TARGET_1516", targets.target)
            .env("PBPS_SCRATCH_1516", targets.scratch);
    }
    cmd.output().unwrap()
}

fn successful(output: Output) -> Output {
    assert_eq!(code(&output), 0, "{}{}", stdout(&output), stderr(&output));
    output
}

/// A login confined to a database it owns on `server`, dropped with it.
struct Confined {
    server: String,
    login: String,
    database: String,
    connection: String,
}

impl Confined {
    fn new(server: &str) -> Self {
        let token = format!("{}_{:08x}", std::process::id(), rand::random::<u32>());
        let login = format!("pbps_cli_s1516_{token}");
        let database = format!("pbps_cli_s1516_{token}");
        let password = format!("p{token}");
        on_server(
            server,
            &format!(
                "CREATE ROLE {login} LOGIN PASSWORD '{password}' NOSUPERUSER NOCREATEDB \
                 NOCREATEROLE; GRANT pg_read_all_settings TO {login}"
            ),
        );
        on_server(
            server,
            &format!("CREATE DATABASE {database} OWNER {login} TEMPLATE template0"),
        );
        let kept: Vec<&str> = server
            .split_whitespace()
            .filter(|word| {
                !word.starts_with("dbname=")
                    && !word.starts_with("user=")
                    && !word.starts_with("password=")
            })
            .collect();
        Self {
            server: server.to_owned(),
            connection: format!(
                "{} dbname={database} user={login} password={password}",
                kept.join(" ")
            ),
            login,
            database,
        }
    }
}

impl Drop for Confined {
    fn drop(&mut self) {
        let _ = try_on_server(
            &self.server,
            &format!("DROP DATABASE IF EXISTS {} WITH (FORCE)", self.database),
        );
        let _ = try_on_server(&self.server, &format!("DROP ROLE IF EXISTS {}", self.login));
    }
}
