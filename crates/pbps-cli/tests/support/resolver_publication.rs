use super::*;

/// A plan the resolver answers, published through the binary (#1516). The
/// target holds one routine `app.f(numeric)` that a default, a CHECK, an
/// index predicate, a view and a routine header call as `app.f(1)`; the
/// revision declares `app.f(integer)`, which captures every one of those
/// calls. The scratch is a confined login in its own database on the
/// target's server: the one layout both pinned servers support.
///
/// The same revision brings a row the target already holds under an
/// `ensure` table's management: a change with no SQL, which the published
/// plan carries as the ordinary one does (#1516 review).
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
    on_server(connection, "INSERT INTO app.country VALUES ('JP', 'Japan')");
    std::fs::write(
        d.dir.join("schema/app.f%28integer%29.function.yml"),
        "function: app.f(integer)\npublic_execute: true\n\ndefinition: |-\n  \
         (integer) RETURNS numeric LANGUAGE sql IMMUTABLE RETURN $1\n",
    )
    .unwrap();
    std::fs::write(
        d.dir.join("schema/app.country.yml"),
        country("\n    TW: {label: Taiwan}\n    JP: {label: Japan}"),
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

    // `--staged` is a gate: its evidence describes one transaction, so it is
    // refused by name before the resolver runs, and nothing is written
    // (#1517).
    let mut args = plan_args(&artifact, &sql);
    args.push("--staged");
    let staged = run(&d, &args, Some(&targets));
    assert_eq!(code(&staged), 2, "{}{}", stdout(&staged), stderr(&staged));
    let value: serde_json::Value = serde_json::from_str(&stdout(&staged)).unwrap();
    let finding = &value["findings"][0];
    assert_eq!(finding["id"], "resolver.unresolved", "{value}");
    assert!(
        finding["message"]
            .as_str()
            .is_some_and(|m| m.contains("--staged cannot carry its answer")),
        "{value}"
    );
    assert_prior(&artifact, &sql);

    // A deployment role that may not read the cluster's identity meets the
    // same gate by name: `--staged` refuses before that read (#1945 review).
    let reader = format!("pbps_cli_r1517_{}", std::process::id());
    let _roles = ClusterRoles {
        server: connection.to_owned(),
        names: vec![reader.clone()],
    };
    on_server(
        connection,
        &format!(
            "CREATE ROLE {reader} LOGIN NOSUPERUSER PASSWORD 'trigger-test'; \
             GRANT pg_read_all_data TO {reader}"
        ),
    );
    let execute = ControlSystemExecuteRevoked::new(connection);
    let limited = as_role(connection, &reader);
    let as_reader = Targets {
        target: &limited,
        scratch: &scratch.connection,
    };
    let mut args = plan_args(&artifact, &sql);
    args.push("--staged");
    let gated = run(&d, &args, Some(&as_reader));
    // Negative: without `--staged` the same role stops at the identity read,
    // so the gate above was reached before it.
    let unread = run(&d, &plan_args(&artifact, &sql), Some(&as_reader));
    drop(execute);
    for (output, id) in [
        (&gated, "resolver.unresolved"),
        (&unread, "resolver.identity"),
    ] {
        assert_eq!(code(output), 2, "{}{}", stdout(output), stderr(output));
        let value: serde_json::Value = serde_json::from_str(&stdout(output)).unwrap();
        assert_eq!(value["findings"][0]["id"], id, "{value}");
    }
    assert_prior(&artifact, &sql);

    // Without either, the plan is published.
    let output = successful(run(&d, &plan_args(&artifact, &sql), Some(&targets)));
    let value: serde_json::Value = serde_json::from_str(&stdout(&output)).unwrap();
    assert_eq!(
        value["data"]["resolver_assessment"]["need"], "resolved",
        "{value}"
    );
    let saved: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&artifact).unwrap()).unwrap();
    assert_eq!(saved["origin"], "resolved_database", "{saved}");
    let typed: pbps_model::SavedPlan = serde_json::from_value(saved.clone()).unwrap();
    let keys: Vec<_> = typed
        .changes
        .changes
        .iter()
        .filter_map(|p| {
            if let pbps_model::Change::SetEnsureKeys { table, from, to } = &p.change {
                Some((table.raw().to_string(), from.clone(), to.clone()))
            } else {
                None
            }
        })
        .collect();
    assert_eq!(
        keys,
        vec![(
            "app.country".to_owned(),
            ["TW"].into_iter().map(Into::into).collect(),
            ["TW", "JP"].into_iter().map(Into::into).collect()
        )],
        "the published plan keeps the newly managed key"
    );
    let cluster = saved["analysis"]["evidence"]["qualification"]["target_cluster"]
        .as_str()
        .unwrap_or_else(|| panic!("the evidence seals the target's cluster: {saved}"));
    assert!(
        cluster.len() == 64 && cluster.bytes().all(|b| b.is_ascii_hexdigit()),
        "keyed, never the identifier in clear: {cluster}"
    );
    assert_exact_order(&std::fs::read_to_string(&sql).unwrap());
    assert_explained(&d, &artifact);

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

/// `explain` of the published plan, from the file alone (#1517): the
/// entry, its scratch kind and layout, each surface's verdict and every
/// edge with its reason, in the JSON and the human form alike. The
/// prerequisites are the whole captured scope, which depends on the
/// cluster, so only their shape is pinned: identities, never a fingerprint.
fn assert_explained(d: &Demo, artifact: &std::path::Path) {
    let plan = artifact.to_str().unwrap();
    let json = successful(run(
        d,
        &["explain", "--plan", plan, "--format", "json"],
        None,
    ));
    let value: serde_json::Value = serde_json::from_str(&stdout(&json)).unwrap();
    let resolver = &value["data"]["resolver"];
    assert_eq!(
        (
            &resolver["entry"],
            &resolver["scratch"],
            &resolver["layout"]
        ),
        (
            &serde_json::json!("scratch"),
            &serde_json::json!("server"),
            &serde_json::json!("supplied")
        ),
        "{resolver}"
    );
    let surfaces: Vec<(String, String)> = resolver["surfaces"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| {
            (
                s["verdict"].as_str().unwrap().to_owned(),
                s["surface"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    assert_eq!(
        surfaces,
        [
            ("rebuilt", "default app.t.c"),
            ("rebuilt", "check app.t.ck"),
            ("rebuilt", "index app.t.ix"),
            ("rebuilt", "module app.v"),
            ("unaffected", "module app.w"),
            ("created", "module app.f(integer)"),
            ("unaffected", "module app.f(numeric)"),
            ("rebuilt", "module app.h(numeric)"),
        ]
        .map(|(v, s)| (v.to_owned(), s.to_owned()))
    );
    let prerequisites = resolver["prerequisites"].as_array().unwrap();
    let object = |class: &str, name: &[&str]| serde_json::json!({"class": class, "name": name, "signature": []});
    for named in [
        object("pg_namespace", &["app"]),
        object("pg_class", &["app", "t"]),
        object("pg_class", &["app", "v"]),
    ] {
        assert!(
            prerequisites
                .iter()
                .any(|p| p["object"] == named && p["fingerprinted"] == true),
            "{named}"
        );
    }
    // The overload the plan creates is sealed only as the closing
    // manifest's placeholder: listed, without a fingerprint (#1517 review).
    let created = serde_json::json!({"class": "pg_proc", "name": ["app", "f"], "signature": [
        {"class": "pg_type", "name": ["pg_catalog", "int4"], "signature": []}
    ]});
    assert!(
        prerequisites
            .iter()
            .any(|p| p["object"] == created && p["fingerprinted"] == false),
        "{created}"
    );
    // Negative: the target held no such overload to fingerprint.
    assert!(
        !prerequisites
            .iter()
            .any(|p| p["object"] == created && p["fingerprinted"] == true),
        "{created}"
    );
    let text = stdout(&successful(run(d, &["explain", "--plan", plan], None)));
    assert!(
        text.contains("  resolver    scratch (server scratch, supplied)\n"),
        "{text}"
    );
    let block = text
        .split_once("\n  surfaces\n")
        .and_then(|(_, rest)| rest.split_once("\n  prerequisites "))
        .map(|(block, _)| block)
        .unwrap_or_else(|| panic!("{text}"));
    let expected = [
        "  surfaces",
        "    rebuilt     default app.t.c",
        "    rebuilt     check app.t.ck",
        "    rebuilt     index app.t.ix",
        "    rebuilt     module app.v",
        "    unaffected  module app.w",
        "    created     module app.f(integer)",
        "    unaffected  module app.f(numeric)",
        "    rebuilt     module app.h(numeric)",
        "  order",
        "    #1 create_module app.f(integer) before #2 public_execution app.f(integer) (structural)",
        "    #1 create_module app.f(integer) before #4 create_module app.v (binding)",
        "    #1 create_module app.f(integer) before #7 create_module app.h(numeric) (binding)",
        "    #1 create_module app.f(integer) before #11 add_check app.t (binding)",
        "    #1 create_module app.f(integer) before #14 alter_column_default app.t (binding)",
        "    #1 create_module app.f(integer) before #15 add_index app.t (binding)",
        "    #2 public_execution app.f(integer) before #11 add_check app.t (authorization)",
        "    #2 public_execution app.f(integer) before #14 alter_column_default app.t (authorization)",
        "    #2 public_execution app.f(integer) before #15 add_index app.t (authorization)",
        "    #3 drop_module app.v before #4 create_module app.v (identity)",
        "    #5 drop_module app.w before #6 drop_module app.h(numeric) (binding)",
        "    #5 drop_module app.w before #9 create_module app.w (identity)",
        "    #6 drop_module app.h(numeric) before #7 create_module app.h(numeric) (identity)",
        "    #7 create_module app.h(numeric) before #8 public_execution app.h(numeric) (structural)",
        "    #7 create_module app.h(numeric) before #9 create_module app.w (binding)",
        "    #10 drop_check app.t before #11 add_check app.t (identity)",
        "    #10 drop_check app.t before #13 alter_column_default app.t (binding)",
        "    #10 drop_check app.t before #14 alter_column_default app.t (binding)",
        "    #12 drop_index app.t before #13 alter_column_default app.t (binding)",
        "    #12 drop_index app.t before #14 alter_column_default app.t (binding)",
        "    #12 drop_index app.t before #15 add_index app.t (identity)",
        "    #13 alter_column_default app.t before #14 alter_column_default app.t (identity)",
    ];
    assert_eq!(
        format!("  surfaces\n{block}"),
        expected.join("\n"),
        "{text}"
    );
    // No fingerprint is shown: the sealed values are 64 hex digits.
    let saved: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(artifact).unwrap()).unwrap();
    let cluster = saved["analysis"]["evidence"]["qualification"]["target_cluster"]
        .as_str()
        .unwrap();
    assert!(!text.contains(cluster) && !stdout(&json).contains(cluster));
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
    std::fs::write(
        d.dir.join("schema/app.country.yml"),
        country("\n    TW: {label: Taiwan}"),
    )
    .unwrap();
}

/// A reference table the project manages with `ensure`, holding `rows`.
fn country(rows: &str) -> String {
    format!(
        "table: app.country\ncolumns:\n  code: {{type: varchar(2), nullable: false}}\n  \
         label: {{type: varchar(40), nullable: false}}\n\
         primary_key: {{name: pk_country, columns: [code]}}\n\
         data:\n  mode: ensure\n  rows: {rows}\n"
    )
}

/// Each surface the arriving overload captures is created again after it,
/// so the engine binds it there, and the view over the rebuilt routine is
/// dropped before it and created after: the statements in the one order the
/// published SQL has. The check, the index and the default share a table and
/// nothing else, so each is dropped and put back beside itself (DEC-1968.2).
/// Each statement is scoped to its schema's write path on its own.
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
        r#"ALTER TABLE "app"."t" ADD CONSTRAINT "ck" CHECK (c > app.f(1) )"#,
        r#"DROP INDEX "app"."ix""#,
        r#"ALTER TABLE "app"."t" ALTER COLUMN "c" DROP DEFAULT"#,
        r#"ALTER TABLE "app"."t" ALTER COLUMN "c" SET DEFAULT app.f(1)"#,
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

/// A target bootstrapped from [`declare`], and a plan the resolver sealed
/// against it that adds `app.f(integer)`, which captures every call of
/// `app.f(1)` (#616).
struct Sealed {
    d: Demo,
    // Dropped after `d`'s commands: the scratch and the target outlive them.
    _scratch: Confined,
    own: OwnDatabase,
    artifact: std::path::PathBuf,
}

impl Sealed {
    /// A [`Sealed`] with `unmanaged` run on the target before the plan.
    fn beside(server: &str, slug: &str, unmanaged: Option<&str>) -> Self {
        Self::revised(
            server,
            slug,
            Revision {
                externals: unmanaged,
                ..Revision::default()
            },
        )
    }

    /// A [`Sealed`] with `revision` besides.
    fn revised(server: &str, slug: &str, revision: Revision<'_>) -> Self {
        let own = OwnDatabase::new(server, slug);
        let connection = own.connection().to_owned();
        on_server(&connection, "CREATE SCHEMA app");
        let scratch = Confined::new(server);
        let d = Demo::new(slug);
        declare(&d);
        write_declarations(&d, revision.bootstrapped);
        successful(run(&d, &["plan"], None));
        d.commit();
        successful(run(&d, &["bootstrap", "--db", &connection], None));
        std::fs::write(
            d.dir.join("schema/app.f%28integer%29.function.yml"),
            "function: app.f(integer)\npublic_execute: true\n\ndefinition: |-\n  \
             (integer) RETURNS numeric LANGUAGE sql IMMUTABLE RETURN $1\n",
        )
        .unwrap();
        write_declarations(&d, revision.files);
        successful(run(&d, &["plan"], None));
        d.commit();
        configure(&d);
        if let Some(sql) = revision.target_only {
            on_server(&connection, sql);
        }
        if let Some(sql) = revision.externals {
            // On the target, and in the resolver's externals file, which
            // stages it on scratch (#1673).
            on_server(&connection, sql);
            std::fs::write(d.dir.join("externals.sql"), format!("{sql};\n")).unwrap();
            let config = d.dir.join("pbps.yml");
            let configured = std::fs::read_to_string(&config).unwrap().replace(
                "    url_env: PBPS_SCRATCH_1516\n",
                "    url_env: PBPS_SCRATCH_1516\n    externals: externals.sql\n",
            );
            std::fs::write(&config, configured).unwrap();
        }
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
        let artifact = d.dir.join("plan.json");
        let sql = d.dir.join("plan.sql");
        let targets = Targets {
            target: &connection,
            scratch: &scratch.connection,
        };
        successful(run(&d, &plan_args(&artifact, &sql), Some(&targets)));
        Self {
            d,
            _scratch: scratch,
            own,
            artifact,
        }
    }

    fn connection(&self) -> &str {
        self.own.connection()
    }

    /// `apply` of `artifact`, approved with its own checksum, with the
    /// scratch out of reach.
    fn apply_command(&self, artifact: &std::path::Path) -> Command {
        self.approved_as(artifact, &plan_checksum(artifact))
    }

    /// [`Self::apply_command`] approving `checksum`.
    fn approved_as(&self, artifact: &std::path::Path, checksum: &str) -> Command {
        let mut cmd = Command::new(BIN);
        cmd.arg("--project")
            .arg(&self.d.dir)
            .args([
                "apply",
                "--env",
                "prod",
                "--plan",
                artifact.to_str().unwrap(),
                "--checksum",
                checksum,
                "--allow",
                "rename,destructive,constraint",
            ])
            .env("PBPS_TARGET_1516", self.connection())
            .env("PBPS_SCRATCH_1516", "host=127.0.0.1 port=1 dbname=unused");
        cmd
    }

    fn apply(&self, artifact: &std::path::Path) -> Output {
        self.apply_command(artifact).output().unwrap()
    }

    /// The plan with `edit` made to its JSON, written beside it.
    fn edited(&self, name: &str, edit: impl FnOnce(&mut serde_json::Value)) -> std::path::PathBuf {
        let mut value: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&self.artifact).unwrap()).unwrap();
        edit(&mut value);
        let path = self.d.dir.join(name);
        std::fs::write(&path, serde_json::to_string_pretty(&value).unwrap()).unwrap();
        path
    }

    /// The states the ledger records as applied, and whether the plan's
    /// first statement's routine exists: a refused apply changes neither. A
    /// refusal under the deployment lock does add a failed attempt to the
    /// ledger's audit trail, as every apply failure there does; that row is
    /// not a state.
    fn untouched(&self) -> (i64, i64) {
        (
            scalar(
                self.connection(),
                "SELECT count(*) FROM public.__pbps_state WHERE kind <> 'failed'",
            ),
            scalar(
                self.connection(),
                "SELECT count(*) FROM pg_catalog.pg_proc WHERE oid = \
                 pg_catalog.to_regprocedure('app.f(integer)')",
            ),
        )
    }
}

/// What a [`Sealed`] plan brings besides `app.f(integer)`.
#[derive(Default)]
struct Revision<'a> {
    /// Run on the target before planning, and staged on the scratch through
    /// the resolver's externals file.
    externals: Option<&'a str>,
    /// Run on the target alone before planning.
    target_only: Option<&'a str>,
    /// Declarations the target is bootstrapped with besides [`declare`]'s,
    /// by path under `schema/`.
    bootstrapped: &'a [(&'a str, &'a str)],
    /// Declarations the revision adds, by path under `schema/`.
    files: &'a [(&'a str, &'a str)],
}

fn write_declarations(d: &Demo, files: &[(&str, &str)]) {
    for (name, body) in files {
        let path = d.dir.join("schema").join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
    }
}

/// A cluster role, dropped after everything declared in it.
struct ClusterRole {
    server: String,
    name: String,
}

impl ClusterRole {
    fn new(server: &str) -> Self {
        let name = format!(
            "pbps_r616_{}_{:08x}",
            std::process::id(),
            rand::random::<u32>()
        );
        on_server(server, &format!("CREATE ROLE {name} NOLOGIN"));
        Self {
            server: server.to_owned(),
            name,
        }
    }
}

impl Drop for ClusterRole {
    fn drop(&mut self) {
        let _ = try_on_server(&self.server, &format!("DROP ROLE IF EXISTS {}", self.name));
    }
}

/// What the plan itself installs is the closing read's to find, under the
/// names the target gives it (#616): an identity sequence the engine names
/// past one the target already holds, which the scratch does not, so the
/// two name it differently; a toasted column; and a grant to a role. The
/// plan applies, closes and records, and the next plan is empty.
pub fn installed(server: &str) {
    // Dropped after `sealed`, whose database holds the role's grant.
    let role = ClusterRole::new(server);
    let grants = format!(
        "role: {}\ngrants:\n  schema::app: [usage]\n  app.n: [select]\n",
        role.name
    );
    let role_file = format!("roles/{}.yml", role.name);
    let sealed = Sealed::revised(
        server,
        "inst616",
        Revision {
            target_only: Some("CREATE SEQUENCE app.n_id_seq"),
            files: &[
                (
                    "app.n.yml",
                    "table: app.n\ncolumns:\n  id: {type: integer, nullable: false, \
                     identity: [1, 1]}\n  note: {type: text}\n\
                     primary_key: {name: pk_n, columns: [id]}\n",
                ),
                (&role_file, &grants),
            ],
            ..Revision::default()
        },
    );
    let before = sealed.untouched();
    successful(sealed.apply(&sealed.artifact));
    assert_eq!(sealed.untouched(), (before.0 + 1, 1));
    // The target named the sequence past its own `n_id_seq`.
    assert_eq!(
        text_of(
            sealed.connection(),
            "SELECT pg_catalog.pg_get_serial_sequence('app.n', 'id')"
        ),
        "app.n_id_seq1"
    );
    assert_eq!(
        text_of(
            sealed.connection(),
            &format!(
                "SELECT pg_catalog.has_table_privilege('{}', 'app.n', 'SELECT')::text",
                role.name
            )
        ),
        "true"
    );
    let next = successful(run(
        &sealed.d,
        &["plan", "--env", "prod", "--format", "json"],
        Some(&Targets {
            target: sealed.connection(),
            scratch: &sealed._scratch.connection,
        }),
    ));
    let value: serde_json::Value = serde_json::from_str(&stdout(&next)).unwrap();
    assert_eq!(value["data"]["changes"], 0, "{value}");
}

/// A rename of a table another, unchanged table's foreign key references
/// closes (#616): the rename rewrites the child's key, its internal RI
/// triggers and its dependency rows, which the parent's change carries as
/// references, so the closing read finds them owned by the child and the
/// plan admits them. The plan applies, records, and the next plan is empty.
pub fn renamed_parent(server: &str) {
    let sealed = Sealed::revised(
        server,
        "rename616",
        Revision {
            bootstrapped: &[
                (
                    "app.p.yml",
                    "table: app.p\ncolumns:\n  id: {type: integer, nullable: false}\n\
                     primary_key: {name: pk_p, columns: [id]}\n",
                ),
                (
                    "app.c.yml",
                    "table: app.c\ncolumns:\n  id: {type: integer, nullable: false}\n  \
                     p_id: {type: integer}\nprimary_key: {name: pk_c, columns: [id]}\n\
                     foreign_keys:\n  fk_c_p:\n    columns: [p_id]\n    references: app.p(id)\n",
                ),
            ],
            // The child's declaration names its parent by the new name; its key
            // is the same key.
            files: &[
                (
                    "app.c.yml",
                    "table: app.c\ncolumns:\n  id: {type: integer, nullable: false}\n  \
                     p_id: {type: integer}\nprimary_key: {name: pk_c, columns: [id]}\n\
                     foreign_keys:\n  fk_c_p:\n    columns: [p_id]\n    references: app.q(id)\n",
                ),
                (
                    "app.p.yml",
                    "table: app.q\nrenamed_from: app.p\ncolumns:\n  id: {type: integer, \
                 nullable: false}\nprimary_key: {name: pk_p, columns: [id]}\n",
                ),
            ],
            ..Revision::default()
        },
    );
    let before = sealed.untouched();
    successful(sealed.apply(&sealed.artifact));
    assert_eq!(sealed.untouched(), (before.0 + 1, 1));
    assert_eq!(
        text_of(
            sealed.connection(),
            "SELECT confrelid::regclass::text FROM pg_catalog.pg_constraint WHERE conname = 'fk_c_p'"
        ),
        "app.q"
    );
    let next = successful(run(
        &sealed.d,
        &["plan", "--env", "prod", "--format", "json"],
        Some(&Targets {
            target: sealed.connection(),
            scratch: &sealed._scratch.connection,
        }),
    ));
    let value: serde_json::Value = serde_json::from_str(&stdout(&next)).unwrap();
    assert_eq!(value["data"]["changes"], 0, "{value}");
}

/// What a plan newly reaches closes too (#616): the first `gin` index on
/// the target reaches the existing access method and operator classes
/// nothing reached before, and a new partitioned table brings its partition
/// key. (A partition's bound is a stored expression the capture does not
/// cover yet, so a resolved plan cannot create one.) The plan applies, records, and the next plan is
/// empty.
pub fn newly_reached(server: &str) {
    let doc = "table: app.doc\ncolumns:\n  id: {type: integer, nullable: false}\n  \
               body: {type: jsonb}\nprimary_key: {name: pk_doc, columns: [id]}\n";
    let indexed = format!("{doc}indexes:\n  doc_gin:\n    columns: [body]\n    method: gin\n");
    let sealed = Sealed::revised(
        server,
        "reach616",
        Revision {
            bootstrapped: &[("app.doc.yml", doc)],
            files: &[
                ("app.doc.yml", &indexed),
                (
                    "app.ev.yml",
                    "table: app.ev\ncolumns:\n  ts: {type: date, nullable: false}\n  \
                 id: {type: integer}\npartition_by: [ts]\n",
                ),
            ],
            ..Revision::default()
        },
    );
    let before = sealed.untouched();
    successful(sealed.apply(&sealed.artifact));
    assert_eq!(sealed.untouched(), (before.0 + 1, 1));
    let next = successful(run(
        &sealed.d,
        &["plan", "--env", "prod", "--format", "json"],
        Some(&Targets {
            target: sealed.connection(),
            scratch: &sealed._scratch.connection,
        }),
    ));
    let value: serde_json::Value = serde_json::from_str(&stdout(&next)).unwrap();
    assert_eq!(value["data"]["changes"], 0, "{value}");
}

/// Every input the evidence seals refuses, before the first statement, when
/// it differs; each refusal names it, runs no DDL and writes no ledger entry
/// (#616 steps 1-5). The unchanged plan then applies.
pub fn refused_before_ddl(server: &str) {
    // An external the resolver stages from its externals file (#1673), and
    // an explicit ACL on the schema, which a grant and its revoke leave as
    // it was: a schema's default ACL is NULL, and no revoke restores that.
    let sealed = Sealed::revised(
        server,
        "refuse616",
        Revision {
            externals: Some(
                "CREATE FUNCTION app.f(text) RETURNS numeric LANGUAGE sql IMMUTABLE RETURN 0",
            ),
            target_only: Some("GRANT USAGE ON SCHEMA app TO PUBLIC"),
            ..Revision::default()
        },
    );
    let before = sealed.untouched();
    assert_eq!(before.1, 0, "the plan has not run");
    let refused = |output: Output, said: &[&str]| {
        assert_eq!(code(&output), 1, "{}{}", stdout(&output), stderr(&output));
        let error = stderr(&output);
        for part in said {
            assert!(error.contains(part), "{part}: {error}");
        }
        assert_eq!(sealed.untouched(), before, "{error}");
    };
    let plan: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&sealed.artifact).unwrap()).unwrap();
    let sealed_key = plan["analysis"]["evidence"]["before"]["key_id"]
        .as_str()
        .unwrap()
        .to_owned();

    // Another key: both identifiers named.
    let key = sealed.d.dir.join("key");
    let kept = std::fs::read(&key).unwrap();
    std::fs::remove_file(&key).unwrap();
    successful(run(
        &sealed.d,
        &["key", "generate", "--out", key.to_str().unwrap()],
        None,
    ));
    refused(
        sealed.apply(&sealed.artifact),
        &[
            &format!("sealed under fingerprint key {sealed_key}, and `prod` has key"),
            "Plan again",
        ],
    );
    std::fs::write(&key, &kept).unwrap();
    // No key at all: the sealed identifier and the remedy.
    let config = sealed.d.dir.join("pbps.yml");
    let configured = std::fs::read_to_string(&config).unwrap();
    std::fs::write(
        &config,
        configured.replace("    fingerprint_key_file: key\n", ""),
    )
    .unwrap();
    refused(
        sealed.apply(&sealed.artifact),
        &[
            &format!("sealed under fingerprint key {sealed_key}, and no key is configured"),
            "Plan again",
        ],
    );
    std::fs::write(&config, &configured).unwrap();

    // An edited evidence version, approved under its own checksum.
    let version = sealed.edited("version.json", |plan| {
        plan["analysis"]["evidence"]["version"] = 4.into();
    });
    refused(
        sealed.apply(&version),
        &["unsupported resolver evidence version"],
    );
    // Another cluster than the planning read's.
    let cluster = sealed.edited("cluster.json", |plan| {
        plan["analysis"]["evidence"]["qualification"]["target_cluster"] = "0".repeat(64).into();
    });
    refused(
        sealed.apply(&cluster),
        &["reaches another cluster than the one this plan was sealed against"],
    );
    // Another server version than the planning read's.
    // The other pinned server's, which the reader accepts, in both
    // manifests: the closing one derives from the opening one.
    let version = sealed.edited("server.json", |plan| {
        for manifest in ["before", "after"] {
            let major = &mut plan["analysis"]["evidence"][manifest]["engine_major"];
            *major = if major.as_u64() == Some(18) { 16 } else { 18 }.into();
        }
    });
    refused(
        sealed.apply(&version),
        &["before its first statement", "server version"],
    );
    // A wrong checksum is the ordinary gate's.
    refused(
        sealed
            .approved_as(&sealed.artifact, &"1".repeat(64))
            .output()
            .unwrap(),
        &["no longer matches the artifact approved"],
    );

    // A target fact: an extension installed since planning, in a schema
    // the project does not manage, which no ordinary check reads.
    on_server(
        sealed.connection(),
        "CREATE SCHEMA ext616; CREATE EXTENSION pg_trgm SCHEMA ext616",
    );
    refused(
        sealed.apply(&sealed.artifact),
        &["before its first statement", "the target's reported facts"],
    );
    on_server(
        sealed.connection(),
        "DROP EXTENSION pg_trgm; DROP SCHEMA ext616",
    );
    // A database setting the deployer's sessions take.
    let database = &sealed.own.name;
    on_server(
        sealed.connection(),
        &format!("ALTER DATABASE \"{database}\" SET statement_timeout = '123s'"),
    );
    refused(
        sealed.apply(&sealed.artifact),
        &["before its first statement"],
    );
    on_server(
        sealed.connection(),
        &format!("ALTER DATABASE \"{database}\" RESET statement_timeout"),
    );
    // A grant on a schema the plan writes in.
    on_server(sealed.connection(), "GRANT CREATE ON SCHEMA app TO PUBLIC");
    refused(
        sealed.apply(&sealed.artifact),
        &[
            "before its first statement",
            "the target deployer's authorization",
        ],
    );
    on_server(
        sealed.connection(),
        "REVOKE CREATE ON SCHEMA app FROM PUBLIC",
    );
    // The staged external, changed on the target since planning.
    on_server(
        sealed.connection(),
        "CREATE OR REPLACE FUNCTION app.f(text) RETURNS numeric LANGUAGE sql IMMUTABLE RETURN 1",
    );
    refused(
        sealed.apply(&sealed.artifact),
        &[
            "before its first statement",
            "pg_proc app.f(pg_catalog.text) changed",
        ],
    );
    on_server(
        sealed.connection(),
        "CREATE OR REPLACE FUNCTION app.f(text) RETURNS numeric LANGUAGE sql IMMUTABLE RETURN 0",
    );
    // A candidate the plan's calls are resolved among: an unmanaged
    // overload, which the drift check and the routine pins leave alone.
    on_server(
        sealed.connection(),
        "CREATE FUNCTION app.f(bigint) RETURNS numeric LANGUAGE sql IMMUTABLE RETURN $1",
    );
    refused(
        sealed.apply(&sealed.artifact),
        &["before its first statement", "the candidates pg_proc app.f"],
    );
    on_server(sealed.connection(), "DROP FUNCTION app.f(bigint)");

    // Negative: the target as it was sealed, applied without the scratch.
    successful(sealed.apply(&sealed.artifact));
    let (entries, routine) = sealed.untouched();
    assert_eq!((entries, routine), (before.0 + 1, 1));
}

/// The closing check, after the plan's statements and before recording
/// (#616 step 8, ADR-0016 case 22): another session commits while the apply
/// waits on a lock past its check before the first statement, at the plan's
/// `ALTER TABLE app.t`. The rebuilt views cannot give that window: apply
/// locks each before its first statement. A change that leaves every managed
/// binding as sealed still rolls the apply back, and so does a closing
/// binding the target does not have.
pub fn closing_refusals(server: &str) {
    // Granted to the deployer mid-apply; dropped after `sealed`.
    let role = ClusterRole::new(server);
    // An overload the project does not manage, among the candidates the
    // plan's calls are resolved over.
    let sealed = Sealed::beside(
        server,
        "close616",
        Some("CREATE FUNCTION app.f(text) RETURNS numeric LANGUAGE sql IMMUTABLE RETURN 0"),
    );
    let before = sealed.untouched();
    // The apply, held by another session's `hold` until `waiting` counts it,
    // while a third commits `change`; then the holder ends with `release`.
    let held = |hold: &str, waiting: &str, release: &str, change: &str| -> Output {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let mut holder = rt.block_on(async {
            let mut c = pbps_db::Conn::connect(pbps_db::Driver::Postgres, sealed.connection())
                .await
                .unwrap();
            c.execute(&format!("BEGIN; {hold}")).await.unwrap();
            c
        });
        let mut child = Drained::spawn(&mut sealed.apply_command(&sealed.artifact));
        wait_for_the_apply("the apply queued behind the holder", &mut child, || {
            scalar(sealed.connection(), waiting) > 0
        });
        on_server(sealed.connection(), change);
        rt.block_on(async { holder.execute(release).await.unwrap() });
        child.wait_with_output()
    };
    // At the plan's `ALTER TABLE app.t`, after every view and routine.
    let interrupted = |change: &str| -> Output {
        held(
            "LOCK TABLE app.t IN ACCESS SHARE MODE",
            "SELECT count(*) FROM pg_locks l JOIN pg_class c ON c.oid = l.relation \
             WHERE c.relname = 't' AND NOT l.granted",
            "COMMIT",
            change,
        )
    };
    let refused = |output: Output, said: &[&str]| {
        assert_eq!(code(&output), 1, "{}{}", stdout(&output), stderr(&output));
        let error = stderr(&output);
        assert!(error.contains("after its last statement"), "{error}");
        for part in said {
            assert!(error.contains(part), "{part}: {error}");
        }
        assert_eq!(sealed.untouched(), before, "rolled back: {error}");
    };

    // An unmanaged overload: no managed binding moves, the candidates do.
    refused(
        interrupted(
            "CREATE FUNCTION app.f(bigint) RETURNS numeric LANGUAGE sql IMMUTABLE RETURN $1",
        ),
        &["the candidates pg_proc app.f"],
    );
    on_server(sealed.connection(), "DROP FUNCTION app.f(bigint)");
    // An untouched candidate's properties: the unmanaged overload, which
    // the managed read-back does not compare.
    refused(
        interrupted(
            "CREATE OR REPLACE FUNCTION app.f(text) RETURNS numeric LANGUAGE sql \
             IMMUTABLE RETURN 1",
        ),
        &["pg_proc app.f(pg_catalog.text) changed"],
    );
    on_server(
        sealed.connection(),
        "CREATE OR REPLACE FUNCTION app.f(text) RETURNS numeric LANGUAGE sql \
         IMMUTABLE RETURN 0",
    );
    // A role granted to the deployer. (A grant on a schema the plan writes
    // in cannot commit here: it waits on the apply's own transaction.)
    refused(
        interrupted(&format!("GRANT {} TO CURRENT_USER", role.name)),
        &["pg_auth_members", &role.name],
    );
    on_server(
        sealed.connection(),
        &format!("REVOKE {} FROM CURRENT_USER", role.name),
    );
    // An installed view's binding: held at the plan's first statement,
    // `CREATE FUNCTION app.f(integer)`, by another session's uncommitted
    // routine of that signature, while an overload of `h` arrives that the
    // view `w` the plan then creates binds instead of the sealed one.
    let new_overload =
        "CREATE FUNCTION app.h(integer) RETURNS numeric LANGUAGE sql IMMUTABLE RETURN $1";
    refused(
        held(
            "CREATE FUNCTION app.f(integer) RETURNS numeric LANGUAGE sql IMMUTABLE RETURN 0",
            "SELECT count(*) FROM pg_locks WHERE locktype = 'transactionid' AND NOT granted",
            "ROLLBACK",
            new_overload,
        ),
        &["pg_rewrite _RETURN(app.w)'s bindings changed"],
    );
    on_server(sealed.connection(), "DROP FUNCTION app.h(integer)");
    // Held instead at the routine locks the apply takes before its check
    // (a rebuilt routine's row), the same arrival is refused before the
    // first statement: the check reads after the locks.
    let output = held(
        "ALTER FUNCTION app.h(numeric) COST 5",
        "SELECT count(*) FROM pg_locks WHERE locktype = 'transactionid' AND NOT granted",
        "ROLLBACK",
        new_overload,
    );
    assert_eq!(code(&output), 1, "{}{}", stdout(&output), stderr(&output));
    let error = stderr(&output);
    assert!(
        error.contains("before its first statement") && error.contains("pg_proc app.h"),
        "{error}"
    );
    assert_eq!(sealed.untouched(), before, "{error}");
    on_server(sealed.connection(), "DROP FUNCTION app.h(integer)");
    // A closing binding the target does not have: the rebuilt view `v` sealed
    // as calling the old overload. The reader of a plan cannot tell a
    // placeholder's bindings from the engine's, so only the closing read
    // refuses it.
    let wrong = sealed.edited("wrong.json", |plan| {
        let records = plan["analysis"]["evidence"]["after"]["prerequisites"]
            .as_array_mut()
            .unwrap();
        let old = records
            .iter()
            .find(|r| {
                r["object"]["class"] == "pg_proc"
                    && r["object"]["name"] == serde_json::json!(["app", "f"])
                    && r["object"]["signature"][0]["name"]
                        == serde_json::json!(["pg_catalog", "numeric"])
            })
            .unwrap_or_else(|| panic!("the old overload is a closing record"))["object"]
            .clone();
        // The view's bindings are its rewrite rule's.
        let view = records
            .iter_mut()
            .find(|r| {
                r["object"]["class"] == "pg_rewrite"
                    && r["object"]["signature"][0]["name"] == serde_json::json!(["app", "v"])
            })
            .unwrap();
        for binding in view["bindings"].as_array_mut().unwrap() {
            if binding["target"]["class"] == "pg_proc" {
                binding["target"] = old.clone();
            }
        }
    });
    refused(
        sealed.apply(&wrong),
        &["pg_rewrite _RETURN(app.v)'s bindings changed"],
    );

    // Negative: uninterrupted, the same plan closes and records.
    successful(sealed.apply(&sealed.artifact));
    assert_eq!(sealed.untouched(), (before.0 + 1, 1));
}

/// A resolved plan applies as it was sealed, through the target connection
/// alone (#616): no resolver runs, no scratch or Docker socket is reached,
/// the ordinary approval command does it, and the next plan is empty. The
/// plan is ADR-0016's motivating pair and its cross-kind case (cases 1 and
/// 24): the arriving `app.f(integer)` captures the unqualified calls of a
/// default, a CHECK, an index predicate, a view and a routine header, each
/// recreated after it, and not the view `u`'s explicit numeric call, which
/// the plan leaves alone.
pub fn applied(server: &str) {
    let sealed = Sealed::revised(
        server,
        "apply616",
        Revision {
            bootstrapped: &[(
                "app.u.view.yml",
                "view: app.u\ndefinition: SELECT app.f(1::numeric) AS x\n",
            )],
            ..Revision::default()
        },
    );
    let script = std::fs::read_to_string(sealed.d.dir.join("plan.sql")).unwrap();
    assert!(script.contains(r#"DROP VIEW "app"."v""#), "{script}");
    assert!(
        !script.contains(r#""app"."u""#),
        "the plan rebuilds `u`: {script}"
    );

    // The apply hooks report a resolved apply as any other.
    let config = sealed.d.dir.join("pbps.yml");
    let applied_hook = sealed.d.dir.join("on_apply.json");
    let attempt_hook = sealed.d.dir.join("on_apply_attempt.json");
    let mut configured = std::fs::read_to_string(&config).unwrap();
    configured.push_str(&format!(
        "hooks:\n  on_apply: \"cat > {}\"\n  on_apply_attempt: \"cat > {}\"\n",
        applied_hook.display(),
        attempt_hook.display()
    ));
    std::fs::write(&config, configured).unwrap();
    // The scratch the plan was made with, and a Docker socket, both listen
    // and are never reached.
    let unreachable = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    unreachable.set_nonblocking(true).unwrap();
    let socket = sealed.d.dir.join("docker.sock");
    // A Unix socket, as Docker's is: this fixture runs on Linux, and
    // `--all-targets` compiles it elsewhere too.
    #[cfg(unix)]
    let docker = {
        let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        listener.set_nonblocking(true).unwrap();
        listener
    };
    let output = successful(
        sealed
            .apply_command(&sealed.artifact)
            .env(
                "PBPS_SCRATCH_1516",
                format!(
                    "host=127.0.0.1 port={} dbname=unused user=unused",
                    unreachable.local_addr().unwrap().port()
                ),
            )
            .env("DOCKER_HOST", format!("unix://{}", socket.display()))
            .output()
            .unwrap(),
    );
    assert_eq!(
        unreachable.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock,
        "apply reached the scratch: {}",
        stdout(&output)
    );
    #[cfg(unix)]
    assert_eq!(
        docker.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock,
        "apply reached the Docker socket: {}",
        stdout(&output)
    );
    let applied: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&applied_hook).unwrap()).unwrap();
    assert!(applied["analysis"]["evidence"].is_object(), "{applied}");
    let attempt: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&attempt_hook).unwrap()).unwrap();
    assert_eq!(attempt["outcome"], "success", "{attempt}");
    assert_eq!(
        attempt["checksum"],
        plan_checksum(&sealed.artifact),
        "{attempt}"
    );
    // Every captured call now binds the new overload, as the plan said, and
    // `u` still binds the old one.
    let binders = |routine: &str| {
        text_of(
            sealed.connection(),
            &format!(
                "SELECT pg_catalog.string_agg(pg_catalog.pg_describe_object(classid, objid, \
                 objsubid), '; ' ORDER BY 1) FROM pg_catalog.pg_depend WHERE refclassid = \
                 'pg_catalog.pg_proc'::pg_catalog.regclass AND refobjid = \
                 'app.f({routine})'::pg_catalog.regprocedure AND deptype = 'n'"
            ),
        )
    };
    let new = binders("integer");
    for surface in [
        "default value for column c of table app.t",
        "constraint ck on table app.t",
        "index app.ix",
        "rule _RETURN on view app.v",
        "function app.h(numeric)",
    ] {
        assert!(new.contains(surface), "{surface}: {new}");
    }
    assert!(!new.contains("view app.u"), "{new}");
    let old = binders("numeric");
    assert!(old.contains("rule _RETURN on view app.u"), "{old}");
    assert!(!old.contains("view app.v"), "{old}");
    let next = successful(run(
        &sealed.d,
        &["plan", "--env", "prod", "--format", "json"],
        Some(&Targets {
            target: sealed.connection(),
            scratch: &sealed._scratch.connection,
        }),
    ));
    let value: serde_json::Value = serde_json::from_str(&stdout(&next)).unwrap();
    assert_eq!(value["data"]["changes"], 0, "{value}");
}

/// RLS is carried by deployment, not scratch's binding namespace (#595).
/// The recipient need not exist on a separate scratch cluster.
pub fn row_security(server: &str, scratch_server: &str) {
    let recipient = ClusterRole::new(server);
    let new_recipient = ClusterRole::new(server);
    let own = OwnDatabase::new(server, "rls_resolver1172");
    let connection = own.connection();
    on_server(
        connection,
        &format!(
            "CREATE SCHEMA app; CREATE TABLE app.t (id integer); \
         CREATE TABLE app.held (id integer); \
         CREATE POLICY retained ON app.held TO {} USING (id > 0); \
         ALTER TABLE app.held ENABLE ROW LEVEL SECURITY; \
         ALTER TABLE app.held FORCE ROW LEVEL SECURITY",
            recipient.name
        ),
    );
    let catalog = "SELECT pg_catalog.jsonb_build_array(c.relrowsecurity,c.relforcerowsecurity, \
        p.polname,p.polcmd,p.polpermissive,p.polroles, \
        pg_catalog.pg_get_expr(p.polqual,p.polrelid), \
        pg_catalog.pg_get_expr(p.polwithcheck,p.polrelid))::text \
        FROM pg_catalog.pg_class c JOIN pg_catalog.pg_policy p ON p.polrelid=c.oid \
        WHERE c.oid='app.held'::regclass";
    let before = text_of(connection, catalog);
    let d = Demo::new("rls-resolver1172");
    successful(run(&d, &["pull", "--db", connection], None));
    d.commit();
    successful(run(
        &d,
        &["baseline", "--db", connection, "--reason", "adopt RLS"],
        None,
    ));
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
    let scratch = Confined::new(scratch_server);
    if server != scratch_server {
        assert_eq!(
            scalar(
                &scratch.connection,
                &format!(
                    "SELECT count(*) FROM pg_catalog.pg_roles WHERE rolname IN ('{}','{}')",
                    recipient.name, new_recipient.name
                )
            ),
            0,
            "the separate scratch cluster has no policy recipient"
        );
    }
    let targets = Targets {
        target: connection,
        scratch: &scratch.connection,
    };
    let artifact = d.dir.join("plan.json");
    let sql = d.dir.join("plan.sql");
    let apply = || {
        successful(run(
            &d,
            &[
                "apply",
                "--env",
                "prod",
                "--plan",
                artifact.to_str().unwrap(),
                "--checksum",
                &plan_checksum(&artifact),
                "--allow",
                "grant-widen,destructive,revoke,row-security",
            ],
            Some(&targets),
        ));
    };

    // A pulled RLS table must not refuse an unrelated column migration.
    let plain = d.dir.join("schema/app.t.yml");
    let original = std::fs::read_to_string(&plain).unwrap();
    std::fs::write(
        &plain,
        original.replace("columns:\n", "columns:\n  label: {type: text}\n"),
    )
    .unwrap();
    // A retained caller gives the arriving integer overload a real
    // resolver question in the next revision.
    write_declarations(
        &d,
        &[
            (
                "app.visible%28numeric%29.function.yml",
                "function: app.visible(numeric)\npublic_execute: true\ndefinition: |-\n  \
          (numeric) RETURNS numeric LANGUAGE sql IMMUTABLE RETURN $1\n",
            ),
            (
                "app.v.view.yml",
                "view: app.v\ndefinition: SELECT app.visible(1) AS n\n",
            ),
        ],
    );
    successful(run(&d, &["plan"], None));
    d.commit();
    let planned = successful(run(&d, &plan_args(&artifact, &sql), Some(&targets)));
    let value: serde_json::Value = serde_json::from_str(&stdout(&planned)).unwrap();
    assert!(value["findings"].to_string().contains("#595"), "{value}");
    let saved: pbps_model::SavedPlan =
        serde_json::from_str(&std::fs::read_to_string(&artifact).unwrap()).unwrap();
    assert!(saved.changes.changes.iter().any(|step| matches!(
        &step.change, pbps_model::Change::AddColumn { table, name, .. }
        if table == &pbps_model::TableName::new("app", "t") && name.raw() == "label"
    )));
    assert!(!saved.changes.changes.iter().any(|step| matches!(
        step.change,
        pbps_model::Change::CreatePolicy { .. } | pbps_model::Change::SetRowSecurity { .. }
    )));
    apply();
    assert_eq!(text_of(connection, catalog), before);

    // New policies and flags remain real deployment steps while scratch
    // compiles their table/routine namespace without claiming policy proof.
    write_declarations(
        &d,
        &[(
            "app.visible%28integer%29.function.yml",
            "function: app.visible(integer)\npublic_execute: true\ndefinition: |-\n  \
         (integer) RETURNS integer LANGUAGE sql IMMUTABLE RETURN $1\n",
        )],
    );
    std::fs::write(
        d.dir.join("schema/app.docs.yml"),
        format!(
            r#"table: app.docs
columns:
  id: {{type: integer}}
row_security: {{enabled: true, forced: true}}
row_policies:
  visible:
    to: [{}]
    using: app.visible(id) > 0
"#,
            new_recipient.name
        ),
    )
    .unwrap();
    successful(run(&d, &["plan"], None));
    d.commit();
    let planned = successful(run(&d, &plan_args(&artifact, &sql), Some(&targets)));
    let value: serde_json::Value = serde_json::from_str(&stdout(&planned)).unwrap();
    assert!(value["findings"].to_string().contains("#595"), "{value}");
    let saved: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&artifact).unwrap()).unwrap();
    assert_eq!(saved["origin"], "resolved_database", "{saved}");
    let saved: pbps_model::SavedPlan = serde_json::from_value(saved).unwrap();
    assert!(
        saved
            .changes
            .changes
            .iter()
            .any(|p| matches!(p.change, pbps_model::Change::CreatePolicy { .. }))
    );
    assert!(
        saved
            .changes
            .changes
            .iter()
            .any(|p| matches!(p.change, pbps_model::Change::SetRowSecurity { .. }))
    );
    apply();
    successful(run(&d, &["verify", "--env", "prod"], Some(&targets)));
    assert_eq!(text_of(connection, catalog), before);
    let noop = successful(run(
        &d,
        &["plan", "--env", "prod", "--format", "json"],
        Some(&targets),
    ));
    let value: serde_json::Value = serde_json::from_str(&stdout(&noop)).unwrap();
    assert_eq!(value["data"]["changes"], 0, "{value}");
    assert!(value["findings"].to_string().contains("#595"), "{value}");

    // Standing replacements are delivered on the target while their opaque
    // predicates still make no binding claim about scratch (#595).
    let held = d.dir.join("schema/app.held.yml");
    let original = std::fs::read_to_string(&held).unwrap();
    let edited = original.replace("id > 0", "id > 1");
    assert_ne!(edited, original);
    std::fs::write(&held, edited).unwrap();
    successful(run(&d, &["plan"], None));
    d.commit();
    write_prior(&artifact, &sql);
    successful(run(&d, &plan_args(&artifact, &sql), Some(&targets)));
    let replacement: pbps_model::SavedPlan =
        serde_json::from_slice(&std::fs::read(&artifact).unwrap()).unwrap();
    assert!(matches!(&replacement.changes.changes[..],[drop,create]
        if matches!(drop.change,pbps_model::Change::DropPolicy {..})
        && matches!(create.change,pbps_model::Change::CreatePolicy {..})));
    assert_eq!(
        replacement.changes.gated_risks(),
        [
            pbps_model::RiskClass::Revoke,
            pbps_model::RiskClass::RowSecurity
        ]
        .into()
    );
    apply();
    successful(run(&d, &["verify", "--env", "prod"], Some(&targets)));
    let after = text_of(connection, catalog);
    assert_ne!(after, before);
    assert!(after.contains("(id > 1)"), "{after}");
    let noop = successful(run(
        &d,
        &["plan", "--env", "prod", "--format", "json"],
        Some(&targets),
    ));
    let value: serde_json::Value = serde_json::from_str(&stdout(&noop)).unwrap();
    assert_eq!(value["data"]["changes"], 0, "{value}");
    assert!(value["findings"].to_string().contains("#595"), "{value}");
}

/// The acceptance project of #2407: a resolver-backed plan whose declarations
/// hold a type. The revision replaces `app.f(numeric)` with `app.f(integer)`,
/// which captures the calls `app.f(1)` and so makes the resolver necessary
/// (the `published` fixture), and adds a view `app.uv` over a column of
/// `column_type`. Where the type is new, the plan creates it and the table
/// too; where it is `existing`, both are on the target and only the view is
/// new.
pub fn declared_types(server: &str) {
    let enum_file = (
        "app.mood.type.yml",
        "enum: app.mood\nvalues: [sad, ok, happy]\n",
    );
    // A schema that holds nothing but the type.
    let own_schema_file = (
        "kinds.mood.type.yml",
        "enum: kinds.mood\nvalues: [sad, ok, happy]\n",
    );
    let domain_file = (
        "app.short.type.yml",
        "domain: app.short\ntype: text\nchecks: {nonempty: \"VALUE <> ''\"}\n",
    );
    // (name, the type's declaration, the column's type, the pg_type the plan
    // leaves behind, whether the type is already on the target). `external`
    // is a type nobody declares, which an externals file stages instead.
    let variants = [
        ("control", None, "text", None, false),
        ("enum", Some(enum_file), "app.mood", Some("mood"), false),
        (
            "domain",
            Some(domain_file),
            "app.short",
            Some("short"),
            false,
        ),
        (
            "enum_existing",
            Some(enum_file),
            "app.mood",
            Some("mood"),
            true,
        ),
        (
            "own_schema",
            Some(own_schema_file),
            "kinds.mood",
            None,
            false,
        ),
        ("external", None, "ext.kind", None, true),
        // The type changes in the same plan: a label is added, then labels
        // are removed and reordered, which rebuilds the enum under its uid.
        (
            "label_added",
            Some(("app.mood.type.yml", "enum: app.mood\nvalues: [sad, ok]\n")),
            "app.mood",
            Some("mood"),
            true,
        ),
        ("rebuilt", Some(enum_file), "app.mood", Some("mood"), true),
    ];
    for (name, type_file, column_type, made, existing) in variants {
        let revised = match name {
            "label_added" => Some("enum: app.mood\nvalues: [sad, ok, happy]\n"),
            "rebuilt" => Some("enum: app.mood\nvalues: [happy, ok]\n"),
            _ => None,
        };
        let external = name == "external";
        let own_schema = name == "own_schema";
        let slug = format!("ctgap_{name}");
        let own = OwnDatabase::new(server, &slug);
        let connection = own.connection();
        on_server(connection, "CREATE SCHEMA app");
        if external {
            on_server(
                connection,
                "CREATE SCHEMA ext; CREATE TYPE ext.kind AS ENUM ('x', 'y')",
            );
        }
        if own_schema {
            on_server(connection, "CREATE SCHEMA kinds");
        }
        let scratch = Confined::new(server);
        let d = Demo::new(&slug);
        declare(&d);
        let table = |d: &Demo| {
            if let Some((file, body)) = type_file {
                std::fs::write(d.dir.join("schema").join(file), body).unwrap();
            }
            std::fs::write(
                d.dir.join("schema/app.u.yml"),
                format!(
                    "table: app.u\ncolumns:\n  id: {{type: integer, nullable: false}}\n  \
                     m: {{type: {column_type}}}\nprimary_key: {{name: pk_u, columns: [id]}}\n"
                ),
            )
            .unwrap();
        };
        let view = |d: &Demo| {
            std::fs::write(
                d.dir.join("schema/app.uv.view.yml"),
                // A constant of the type binds it by name, besides the
                // column's type, which binds it by identity.
                if column_type == "text" || column_type == "ext.kind" {
                    "view: app.uv\ndefinition: SELECT id, m FROM app.u\n".to_owned()
                } else {
                    let constant = if column_type == "app.short" { "'x'" } else { "'ok'" };
                    format!(
                        "view: app.uv\ndefinition: SELECT id, m, {constant}::{column_type} AS k FROM app.u\n"
                    )
                },
            )
            .unwrap();
        };
        if existing {
            table(&d);
        }
        successful(run(&d, &["plan"], None));
        d.commit();
        successful(run(&d, &["bootstrap", "--db", connection], None));
        std::fs::write(
            d.dir.join("schema/app.f%28integer%29.function.yml"),
            "function: app.f(integer)\npublic_execute: true\n\ndefinition: |-\n  \
             (integer) RETURNS numeric LANGUAGE sql IMMUTABLE RETURN $1\n",
        )
        .unwrap();
        if !existing {
            table(&d);
        }
        if let (Some(body), Some((file, _))) = (revised, type_file) {
            std::fs::write(d.dir.join("schema").join(file), body).unwrap();
        }
        view(&d);
        successful(run(&d, &["plan"], None));
        d.commit();
        configure(&d);
        if external {
            // The resolver builds the type the project reads but does not
            // declare from its externals file, as it always has.
            std::fs::create_dir_all(d.dir.join("db")).unwrap();
            std::fs::write(
                d.dir.join("db/externals.sql"),
                "CREATE SCHEMA ext;\nCREATE TYPE ext.kind AS ENUM ('x', 'y');\n",
            )
            .unwrap();
            let config = d.dir.join("pbps.yml");
            let text = std::fs::read_to_string(&config).unwrap();
            std::fs::write(
                &config,
                text.replace(
                    "    url_env: PBPS_SCRATCH_1516\n",
                    "    url_env: PBPS_SCRATCH_1516\n    externals: db/externals.sql\n",
                ),
            )
            .unwrap();
        }
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
        let out = run(&d, &plan_args(&artifact, &sql), Some(&targets));
        assert_eq!(code(&out), 0, "{name}: {}{}", stdout(&out), stderr(&out));
        // Pre-flight lines can precede the document.
        let text = stdout(&out);
        let json: serde_json::Value = serde_json::from_str(&text[text.find('{').unwrap()..])
            .unwrap_or_else(|e| panic!("{name}: {e}: {text}{}", stderr(&out)));
        assert_eq!(
            json["data"]["resolver_assessment"]["need"], "resolved",
            "{name}"
        );
        // It seals and applies, and the view stands over the column.
        successful(run(
            &d,
            &[
                "apply",
                "--env",
                "prod",
                "--plan",
                artifact.to_str().unwrap(),
                "--checksum",
                &plan_checksum(&artifact),
                "--allow",
                "grant-widen,destructive,constraint,type-rebuild",
            ],
            Some(&targets),
        ));
        assert_eq!(
            scalar(
                connection,
                "SELECT count(*) FROM pg_catalog.pg_views WHERE viewname = 'uv'"
            ),
            1,
            "{name}"
        );
        if let Some(made) = made {
            assert_eq!(
                scalar(
                    connection,
                    &format!(
                        "SELECT count(*) FROM pg_catalog.pg_type WHERE typname = '{made}' \
                         AND typnamespace = 'app'::pg_catalog.regnamespace"
                    )
                ),
                1,
                "{name}"
            );
        }
        // And the next plan has nothing left to do.
        let next = successful(run(
            &d,
            &["plan", "--env", "prod", "--format", "json"],
            Some(&targets),
        ));
        let text = stdout(&next);
        let value: serde_json::Value =
            serde_json::from_str(&text[text.find('{').unwrap()..]).unwrap();
        assert_eq!(value["data"]["changes"], 0, "{name}: {value}");
    }
}
