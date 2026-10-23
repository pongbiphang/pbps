//! The same historical-state contract on SQL Server and both PostgreSQL versions.
use super::{Demo, OwnDatabase, code, on_server, stderr, stdout, try_on_server};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Output;

fn ok(out: Output) -> Output {
    assert_eq!(code(&out), 0, "{}{}", stdout(&out), stderr(&out));
    out
}

fn json(out: Output) -> serde_json::Value {
    let report = serde_json::from_slice(&out.stdout)
        .unwrap_or_else(|error| panic!("{error}: {}{}", stdout(&out), stderr(&out)));
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../schemas/envelope.schema.json");
    let schema: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    let validator = jsonschema::validator_for(&schema).unwrap();
    assert!(
        validator.is_valid(&report),
        "report does not match the published envelope: {report}"
    );
    report
}

fn ledger(pg: bool) -> &'static str {
    if pg {
        "public.__pbps_state"
    } else {
        "dbo.__pbps_state"
    }
}

fn records(connection: &str, pg: bool) -> Vec<(i64, String)> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let driver = if pg {
                pbps_db::Driver::Postgres
            } else {
                pbps_db::Driver::Mssql
            };
            let mut conn = pbps_db::Conn::connect(driver, connection).await.unwrap();
            conn.query(&format!(
                "SELECT id, state_json FROM {} ORDER BY id",
                ledger(pg)
            ))
            .await
            .unwrap()
            .into_iter()
            .map(|row| {
                (
                    row.try_get::<i64>("id").unwrap().unwrap(),
                    row.try_get::<&str>("state_json")
                        .unwrap()
                        .unwrap()
                        .to_owned(),
                )
            })
            .collect()
        })
}

fn set_record(connection: &str, pg: bool, id: i64, text: &str) {
    let prefix = if pg { "" } else { "N" };
    on_server(
        connection,
        &format!(
            "UPDATE {} SET state_json = {prefix}'{}' WHERE id = {id}",
            ledger(pg),
            text.replace('\'', "''")
        ),
    );
}

fn project_bytes(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    fn walk(root: &Path, here: &Path, files: &mut BTreeMap<PathBuf, Vec<u8>>) {
        for entry in std::fs::read_dir(here).unwrap() {
            let path = entry.unwrap().path();
            if path.file_name().unwrap() == ".git" {
                continue;
            }
            if path.is_dir() {
                walk(root, &path, files);
            } else {
                files.insert(
                    path.strip_prefix(root).unwrap().to_owned(),
                    std::fs::read(path).unwrap(),
                );
            }
        }
    }
    let mut files = BTreeMap::new();
    walk(root, root, &mut files);
    files
}

fn table(schema: &str, later: bool) -> String {
    let default = if later { "Fresh" } else { "Tea" };
    let value = if later { "New row" } else { "Old row" };
    // Byte-for-byte plan parity needs the engine's type spelling. Values
    // distinct from either default also avoid the snapshot's DEFAULT/value
    // ambiguity; exporting recorded omitted cells is tested by round trips.
    let varchar = if schema == "app" {
        "character varying"
    } else {
        "varchar"
    };
    format!(
        concat!(
            "table: {schema}.product\ncolumns:\n",
            "  code: {{type: {varchar}(20), nullable: false}}\n",
            "  label: {{type: {varchar}(40), nullable: false, default: \"'{default}'\"}}\n",
            "primary_key: {{name: pk_product, columns: [code]}}\n",
            "checks:\n  ck_label: \"label <> ''\"\n",
            "indexes:\n  ix_label: {{columns: [label], where: \"label <> 'none'\"}}\n",
            "data:\n  mode: exact\n  rows:\n    TEA: {{label: {value}}}\n    DEF: {{}}\n"
        ),
        schema = schema,
        varchar = varchar,
        default = default,
        value = value,
    )
}

fn prepared(pg: bool, name: &str) -> Demo {
    let d = Demo::new(name);
    let schema = if pg { "app" } else { "dbo" };
    std::fs::write(d.dir.join("schema/product.yml"), table(schema, false)).unwrap();
    ok(d.run(&["plan", "--no-dev"]));
    d.commit();
    d
}

fn plan(d: &Demo, connection: &str, name: &str) -> pbps_model::SavedPlan {
    let path = d.dir.join(name);
    ok(d.run(&["plan", "--db", connection, "--out", path.to_str().unwrap()]));
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

fn apply(d: &Demo, connection: &str, name: &str, plan: &pbps_model::SavedPlan) {
    let path = d.dir.join(name);
    let checksum = plan.wire_fingerprint();
    let risks = plan
        .changes
        .gated_risks()
        .iter()
        .map(|r| r.as_str())
        .collect::<Vec<_>>()
        .join(",");
    let mut args = vec![
        "apply",
        "--db",
        connection,
        "--plan",
        path.to_str().unwrap(),
        "--checksum",
        &checksum,
    ];
    if !risks.is_empty() {
        args.extend(["--allow", &risks]);
    }
    ok(d.run(&args));
    ok(d.run(&["verify", "--db", connection]));
}

struct Role {
    server: String,
    name: String,
    pg: bool,
}
impl Drop for Role {
    fn drop(&mut self) {
        if self.pg {
            let _ = try_on_server(&self.server, &format!("DROP ROLE {}", self.name));
        }
    }
}

pub fn recovery(server: &str, pg: bool) {
    let role = Role {
        server: server.to_owned(),
        name: format!("pbps_history2289_{}", std::process::id()),
        pg,
    };
    if pg {
        on_server(server, &format!("CREATE ROLE {} NOLOGIN", role.name));
    }
    let own = OwnDatabase::new(server, "history2289");
    let connection = own.connection();
    let schema = if pg { "app" } else { "dbo" };
    if pg {
        on_server(connection, "CREATE SCHEMA app");
    }
    let d = Demo::new("history2289");
    std::fs::write(d.dir.join("schema/product.yml"), table(schema, false)).unwrap();
    std::fs::write(
        d.dir.join("schema/view.yml"),
        format!(
            "view: {schema}.products\ndefinition: |-\n  SELECT code, label FROM {schema}.product\n"
        ),
    )
    .unwrap();
    let routine = if pg { "app.answer()" } else { "dbo.answer" };
    let definition = if pg {
        "() RETURNS integer LANGUAGE sql AS $$ SELECT 7 $$"
    } else {
        "() RETURNS int AS BEGIN RETURN 7 END"
    };
    std::fs::write(
        d.dir.join("schema/function.yml"),
        format!(
            "function: {routine}\ndepends_on: [{schema}.products]\ndefinition: |-\n  {definition}\n"
        ),
    )
    .unwrap();
    std::fs::write(
        d.dir.join("schema/reader.yml"),
        format!(
            "role: {}\ngrants:\n  {schema}.product: [select]\n{}",
            role.name,
            if pg { "  schema::app: [usage]\n" } else { "" }
        ),
    )
    .unwrap();
    let declaration = format!(
        concat!(
            "table: {schema}.ensured\ncolumns:\n",
            "  code: {{type: int, nullable: false}}\n  label: {{type: varchar(40)}}\n",
            "primary_key: {{name: pk_ensured, columns: [code]}}\n",
            "data:\n  mode: ensure\n  rows:\n    '1': {{label: Declared}}\n"
        ),
        schema = schema,
    );
    std::fs::write(d.dir.join("schema/ensured.yml"), declaration).unwrap();
    if pg {
        let declaration = concat!(
            "table: app.secured\ncolumns:\n  id: {type: int, nullable: false}\n",
            "primary_key: {name: pk_secured, columns: [id]}\n",
            "row_security: {enabled: true, forced: false}\nrow_policies:\n",
            "  readers: {command: select, to: public, using: 'id > 0'}\n"
        );
        std::fs::write(d.dir.join("schema/secured.yml"), declaration).unwrap();
    }
    ok(d.run(&["plan", "--no-dev"]));
    d.commit();
    ok(d.run(&["bootstrap", "--db", connection]));
    on_server(
        connection,
        &format!("INSERT INTO {schema}.ensured (code, label) VALUES (2, 'Business')"),
    );
    let first = records(connection, pg);
    let id1 = first[0].0.to_string();
    let before = project_bytes(&d.dir);
    let shown = json(ok(d.run(&[
        "state", "show", &id1, "--db", connection, "--format", "json",
    ])));
    assert_eq!(shown["data"]["entry"]["id"], first[0].0);
    assert_eq!(shown["data"]["entry"]["snapshot"]["kind"], "bootstrap");
    for field in ["operator", "git_sha", "plan_checksum"] {
        let snapshot: serde_json::Value = serde_json::from_str(&first[0].1).unwrap();
        assert_eq!(shown["data"]["entry"]["snapshot"][field], snapshot[field]);
    }
    assert!(
        shown["data"]["entry"]["applied_at"]
            .as_str()
            .unwrap()
            .contains('T')
    );
    assert_eq!(project_bytes(&d.dir), before, "show wrote project files");

    std::fs::write(d.dir.join("schema/product.yml"), table(schema, true)).unwrap();
    std::fs::write(
        d.dir.join("schema/function.yml"),
        format!(
            "function: {routine}\ndepends_on: [{schema}.products]\ndefinition: |-\n  {}\n",
            definition.replace('7', "8")
        ),
    )
    .unwrap();
    d.commit();
    let forward = plan(&d, connection, "forward.json");
    assert!(!forward.changes.is_empty());
    apply(&d, connection, "forward.json", &forward);
    let rows = records(connection, pg);
    let id2 = rows.last().unwrap().0.to_string();
    let compared = json(ok(d.run(&[
        "state", "diff", &id1, &id2, "--db", connection, "--format", "json",
    ])));
    assert_eq!(
        compared["data"]["changes"],
        serde_json::to_value(&forward.changes).unwrap()
    );
    let newest = json(ok(d.run(&[
        "state", "diff", &id1, "--db", connection, "--format", "json",
    ])));
    assert_eq!(newest["data"]["to_id"], rows.last().unwrap().0);
    assert_eq!(compared["data"]["changes"], newest["data"]["changes"]);
    let identical = json(ok(d.run(&[
        "state", "diff", &id2, &id2, "--db", connection, "--format", "json",
    ])));
    assert_eq!(
        identical["data"]["changes"]["changes"]
            .as_array()
            .unwrap()
            .len(),
        0
    );
    let ids = std::fs::read(d.dir.join("schema.ids.json")).unwrap();
    let exported = json(ok(d.run(&[
        "state", "export", &id2, "--db", connection, "--force", "--format", "json",
    ])));
    if pg {
        assert!(
            exported["findings"]
                .as_array()
                .unwrap()
                .iter()
                .any(|f| f["id"] == "state.export-public-execute")
        );
    }
    ok(d.run(&["fmt", "--check"]));
    let loaded = pbps_load::load_schema_dir(&d.dir.join("schema")).unwrap();
    assert!(
        loaded
            .hints
            .module_deps
            .values()
            .any(|deps| !deps.is_empty())
    );
    assert!(loaded.hints.public_execute.is_empty());
    assert!(loaded.schema.tables.values().any(|t| t.data.is_some()));
    let ensured = loaded
        .schema
        .tables
        .values()
        .filter_map(|t| t.data.as_ref())
        .find(|d| d.mode == pbps_model::DataMode::Ensure)
        .unwrap();
    assert_eq!(
        ensured.rows.len(),
        1,
        "an ensured business row is not recorded state"
    );
    if pg {
        assert!(loaded.schema.tables.values().any(|t| t.has_row_security()));
    }
    assert_eq!(loaded.schema.roles.len(), 1);
    let product: pbps_model::TableName = format!("{schema}.product").parse().unwrap();
    let default_row = &loaded.schema.tables[&product].data.as_ref().unwrap().rows
        [&pbps_model::RowKey("DEF".into())];
    assert!(
        default_row.get("label").is_none(),
        "recorded omitted cells stay omitted"
    );
    assert_eq!(std::fs::read(d.dir.join("schema.ids.json")).unwrap(), ids);
    d.commit();
    assert!(plan(&d, connection, "roundtrip.json").changes.is_empty());

    let reverse = json(ok(d.run(&[
        "state", "diff", &id2, &id1, "--db", connection, "--format", "json",
    ])));
    let human = ok(d.run(&["state", "export", &id1, "--db", connection, "--force"]));
    if pg {
        assert!(stdout(&human).contains("Snapshots do not record historical public_execute"));
    }
    assert!(stdout(&human).contains("schema.ids.json is unchanged"));
    assert_eq!(std::fs::read(d.dir.join("schema.ids.json")).unwrap(), ids);
    ok(d.run(&["fmt", "--check"]));
    d.commit();
    let backward = plan(&d, connection, "backward.json");
    assert_eq!(
        reverse["data"]["changes"],
        serde_json::to_value(&backward.changes).unwrap()
    );
    assert!(
        backward
            .changes
            .changes
            .iter()
            .any(|p| matches!(p.change, pbps_model::Change::AlterColumnDefault { .. }))
    );
    apply(&d, connection, "backward.json", &backward);
    assert!(plan(&d, connection, "recovered.json").changes.is_empty());
    assert_eq!(
        records(connection, pg).len(),
        3,
        "historical commands must not append ledger rows"
    );
}

fn refused(d: &Demo, connection: &str, args: &[&str], expected: &str, words: &str) {
    let before = project_bytes(&d.dir);
    let mut command = args.to_vec();
    command.extend(["--db", connection, "--format", "json"]);
    let out = d.run(&command);
    assert_eq!(code(&out), 1, "{}{}", stdout(&out), stderr(&out));
    let report = json(out);
    assert_eq!(report["result"], "unanswerable", "{report}");
    let finding = &report["findings"][0];
    assert_eq!(finding["id"], expected, "{report}");
    assert!(
        finding["message"]
            .as_str()
            .unwrap()
            .to_lowercase()
            .contains(words),
        "{report}"
    );
    assert!(
        report.get("data").is_none(),
        "no historical answer was read: {report}"
    );
    assert_eq!(
        project_bytes(&d.dir),
        before,
        "{expected} changed the project"
    );
}

pub fn refusals(server: &str, pg: bool) {
    let own = OwnDatabase::new(server, "history_refusals2289");
    let connection = own.connection();
    let d = prepared(pg, "history-refusals2289");
    for command in ["show", "diff", "export"] {
        refused(
            &d,
            connection,
            &["state", command, "1"],
            "state.uninitialized",
            "no pbps ledger",
        );
    }
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let driver = if pg {
                pbps_db::Driver::Postgres
            } else {
                pbps_db::Driver::Mssql
            };
            let mut conn = pbps_db::Conn::connect(driver, connection).await.unwrap();
            if pg {
                pbps_pg::state::lock(&mut conn, "history2289")
                    .await
                    .unwrap();
                pbps_pg::state::unlock(&mut conn).await.unwrap();
            } else {
                pbps_mssql::state::lock(&mut conn, "history2289")
                    .await
                    .unwrap();
                pbps_mssql::state::unlock(&mut conn).await.unwrap();
            }
        });
    for command in ["show", "diff", "export"] {
        refused(
            &d,
            connection,
            &["state", command, "1"],
            "state.no-entries",
            "ledger exists but has no entries",
        );
    }
    if pg {
        on_server(connection, "CREATE SCHEMA app");
    }
    ok(d.run(&["bootstrap", "--db", connection]));
    let original = records(connection, pg);
    let (id, text) = &original[0];
    let id_text = id.to_string();
    for command in ["show", "diff", "export"] {
        refused(
            &d,
            connection,
            &["state", command, "999999"],
            "state.entry-not-found",
            "no entry #999999",
        );
        set_record(connection, pg, *id, "{not json");
        refused(
            &d,
            connection,
            &["state", command, &id_text],
            "state.unreadable",
            "snapshot is unreadable",
        );
        set_record(connection, pg, *id, "{\"version\":999999}");
        refused(
            &d,
            connection,
            &["state", command, &id_text],
            "state.entry-unsupported-version",
            "snapshot is unreadable",
        );
        let mut invalid_ids: serde_json::Value = serde_json::from_str(text).unwrap();
        invalid_ids["ids"]["version"] = 999999.into();
        set_record(connection, pg, *id, &invalid_ids.to_string());
        refused(
            &d,
            connection,
            &["state", command, &id_text],
            "state.unreadable",
            "inconsistent identity mapping",
        );
        set_record(connection, pg, *id, text);
    }
    let mut old: serde_json::Value = serde_json::from_str(text).unwrap();
    old["version"] = (pbps_model::state::CURRENT_VERSION - 1).into();
    set_record(connection, pg, *id, &old.to_string());
    ok(d.run(&["state", "show", &id_text, "--db", connection]));
    refused(
        &d,
        connection,
        &["state", "export", &id_text, "--force"],
        "state.export-version",
        "state export requires version",
    );
    set_record(connection, pg, *id, text);
    refused(
        &d,
        connection,
        &["state", "export", &id_text],
        "state.export-overwrite",
        "re-run with --force",
    );
    // Staging is the guard: parsing a sound snapshot alone is insufficient.
    let mut invalid = pbps_model::StateSnapshot::from_json(text).unwrap();
    invalid
        .schema
        .tables
        .values_mut()
        .next()
        .unwrap()
        .primary_key
        .as_mut()
        .unwrap()
        .columns
        .clear();
    set_record(
        connection,
        pg,
        *id,
        &serde_json::to_string(&invalid).unwrap(),
    );
    refused(
        &d,
        connection,
        &["state", "export", &id_text, "--force"],
        "state.export-invalid",
        "nothing was written",
    );
    set_record(connection, pg, *id, text);
    // A damaged unrelated row does not hide a readable selected snapshot.
    let prefix = if pg { "" } else { "N" };
    on_server(
        connection,
        &format!(
            "INSERT INTO {} (kind, operator, state_json) VALUES ('apply', 'broken', {prefix}'{{broken')",
            ledger(pg)
        ),
    );
    ok(d.run(&["state", "show", &id_text, "--db", connection]));
    ok(d.run(&["state", "diff", &id_text, &id_text, "--db", connection]));
    ok(d.run(&["state", "export", &id_text, "--force", "--db", connection]));
    // Force removes stale declarations, but never neighboring project files.
    assert!(!d.dir.join("schema/product.yml").exists());
    assert!(d.dir.join("pbps.yml").is_file());
    assert!(d.dir.join("schema.ids.json").is_file());
}

struct Login {
    server: String,
    name: String,
    pg: bool,
}
impl Drop for Login {
    fn drop(&mut self) {
        let kind = if self.pg { "ROLE" } else { "LOGIN" };
        let _ = try_on_server(&self.server, &format!("DROP {kind} {}", self.name));
    }
}

fn reader(connection: &str, name: &str, password: &str, pg: bool) -> String {
    if pg {
        let base = connection
            .split_whitespace()
            .filter(|p| !p.starts_with("user=") && !p.starts_with("password="))
            .collect::<Vec<_>>()
            .join(" ");
        format!("{base} user={name} password={password}")
    } else {
        let base = connection
            .split(';')
            .filter(|part| {
                !part.trim().is_empty()
                    && !part.split_once('=').is_some_and(|(key, _)| {
                        matches!(
                            key.trim().to_lowercase().as_str(),
                            "user id" | "uid" | "user" | "password" | "pwd"
                        )
                    })
            })
            .collect::<Vec<_>>()
            .join(";");
        format!("{base};User Id={name};Password={password}")
    }
}

pub fn permissions(server: &str, pg: bool) {
    let login = Login {
        server: server.to_owned(),
        name: format!("pbps_histreader2289_{}", std::process::id()),
        pg,
    };
    let own = OwnDatabase::new(server, "history_permissions2289");
    let connection = own.connection();
    if pg {
        on_server(connection, "CREATE SCHEMA app");
    }
    let d = prepared(pg, "history-permissions2289");
    ok(d.run(&["bootstrap", "--db", connection]));
    let original = records(connection, pg);
    let id = original[0].0.to_string();
    let password = "History2289!Reader";
    if pg {
        on_server(
            server,
            &format!(
                "CREATE ROLE {} LOGIN NOSUPERUSER PASSWORD '{password}'",
                login.name
            ),
        );
        on_server(
            connection,
            &format!("GRANT USAGE ON SCHEMA public TO {}", login.name),
        );
    } else {
        on_server(
            server,
            &format!(
                "CREATE LOGIN {} WITH PASSWORD = '{password}', CHECK_POLICY = OFF",
                login.name
            ),
        );
        on_server(
            connection,
            &format!("CREATE USER {} FOR LOGIN {}", login.name, login.name),
        );
    }
    let reader = reader(connection, &login.name, password, pg);
    for command in ["show", "diff", "export"] {
        refused(
            &d,
            &reader,
            &["state", command, &id],
            "state.unreadable",
            "ledger could not be read",
        );
    }
    // Reading metadata can succeed while the snapshot column remains denied.
    let cols = concat!(
        "id, applied_at, kind, git_sha, plan_checksum, operator, reason, ",
        "state_version, tables_count, modules_count, staged_completed, staged_total"
    );
    let grant = if pg {
        format!("GRANT SELECT ({cols}) ON {} TO {}", ledger(pg), login.name)
    } else {
        format!("GRANT SELECT ON {} ({cols}) TO {}", ledger(pg), login.name)
    };
    on_server(connection, &grant);
    ok(d.run(&["state", "list", "--db", &reader]));
    refused(
        &d,
        &reader,
        &["state", "export", &id, "--force"],
        "state.unreadable",
        "permission",
    );
    on_server(
        connection,
        &format!("GRANT SELECT ON {} TO {}", ledger(pg), login.name),
    );
    for command in ["show", "diff"] {
        ok(d.run(&["state", command, &id, "--db", &reader]));
    }
    ok(d.run(&["state", "export", &id, "--db", &reader, "--force"]));
    assert!(
        try_on_server(&reader, &format!("DELETE FROM {}", ledger(pg))).is_err(),
        "reader must not be able to mutate the ledger"
    );
    assert_eq!(records(connection, pg), original);
}

pub fn hostile_names(server: &str, pg: bool) {
    let own = OwnDatabase::new(server, "history_hostile2289");
    let connection = own.connection();
    let d = Demo::new("history-hostile2289");
    let schema = if pg { "app" } else { "dbo" };
    if pg {
        on_server(connection, "CREATE SCHEMA app");
    }
    let hostile = "line\ncontrol\u{1b}bidi\u{202e}literal\\n";
    let quote = |name: &str| {
        if pg {
            format!("\"{}\"", name.replace('"', "\"\""))
        } else {
            format!("[{}]", name.replace(']', "]]"))
        }
    };
    on_server(
        connection,
        &format!(
            "CREATE TABLE {schema}.{} ({} int NOT NULL)",
            quote(hostile),
            quote(hostile)
        ),
    );
    ok(d.run(&["pull", "--db", connection]));
    d.commit();
    ok(d.run(&[
        "baseline",
        "--db",
        connection,
        "--reason",
        "hostile identity fixture",
    ]));
    let entries = records(connection, pg);
    let id = entries[0].0.to_string();
    let human = ok(d.run(&["state", "show", &id, "--db", connection]));
    let shown = pbps_model::Text::new(hostile).shown().to_string();
    assert!(stdout(&human).contains(&shown), "{}", stdout(&human));
    assert!(!stdout(&human).contains('\u{1b}') && !stdout(&human).contains('\u{202e}'));
    let report =
        json(ok(d.run(&[
            "state", "show", &id, "--db", connection, "--format", "json",
        ])));
    let tables = report["data"]["entry"]["snapshot"]["schema"]["tables"]
        .as_object()
        .unwrap();
    assert!(
        tables
            .values()
            .any(|t| t["columns"].as_object().unwrap().contains_key(hostile))
    );
    let ids = std::fs::read(d.dir.join("schema.ids.json")).unwrap();
    ok(d.run(&["state", "export", &id, "--db", connection, "--force"]));
    let loaded = pbps_load::load_schema_dir(&d.dir.join("schema")).unwrap();
    assert!(
        loaded
            .schema
            .tables
            .values()
            .any(|t| t.columns.contains_key(hostile))
    );
    assert_eq!(std::fs::read(d.dir.join("schema.ids.json")).unwrap(), ids);
    ok(d.run(&["fmt", "--check"]));
    d.git(&["add", "-A"]);
    d.git(&["commit", "--allow-empty", "-qm", "review exported state"]);
    assert!(plan(&d, connection, "hostile.json").changes.is_empty());
}

pub fn identities(server: &str, pg: bool) {
    let own = OwnDatabase::new(server, "history_ids2289");
    let connection = own.connection();
    if pg {
        on_server(connection, "CREATE SCHEMA app");
    }
    let schema = if pg { "app" } else { "dbo" };
    let d = prepared(pg, "history-ids2289");
    ok(d.run(&["bootstrap", "--db", connection]));
    let first = records(connection, pg)[0].0.to_string();
    let path = d.dir.join("schema/product.yml");
    let renamed = std::fs::read_to_string(&path)
        .unwrap()
        .replace("label", "title");
    std::fs::write(&path, renamed).unwrap();
    ok(d.run(&["rename", &format!("{schema}.product.label"), "title"]));
    d.commit();
    let forward = plan(&d, connection, "rename.json");
    apply(&d, connection, "rename.json", &forward);
    let second = records(connection, pg).last().unwrap().0.to_string();
    let compared = json(ok(d.run(&[
        "state", "diff", &first, &second, "--db", connection, "--format", "json",
    ])));
    assert_eq!(
        compared["data"]["changes"],
        serde_json::to_value(&forward.changes).unwrap()
    );
    assert!(
        forward
            .changes
            .changes
            .iter()
            .any(|p| matches!(p.change, pbps_model::Change::RenameColumn { .. }))
    );
    let ids = std::fs::read(d.dir.join("schema.ids.json")).unwrap();
    ok(d.run(&["state", "export", &first, "--db", connection, "--force"]));
    assert_eq!(std::fs::read(d.dir.join("schema.ids.json")).unwrap(), ids);
    let refused = d.run(&["plan", "--db", connection, "--no-input", "--format", "json"]);
    assert_eq!(
        code(&refused),
        2,
        "export must leave new rename intent to the operator: {}{}",
        stdout(&refused),
        stderr(&refused)
    );
    ok(d.run(&["rename", &format!("{schema}.product.title"), "label"]));
    d.commit();
    let backward = plan(&d, connection, "rename-back.json");
    assert!(
        backward
            .changes
            .changes
            .iter()
            .any(|p| matches!(p.change, pbps_model::Change::RenameColumn { .. }))
    );
    assert!(!backward.changes.changes.iter().any(|p| matches!(
        p.change,
        pbps_model::Change::DropColumn { .. } | pbps_model::Change::AddColumn { .. }
    )));
    apply(&d, connection, "rename-back.json", &backward);
    assert!(plan(&d, connection, "rename-clean.json").changes.is_empty());
}

fn column_fixture(server: &str, pg: bool, name: &str) -> (OwnDatabase, Demo) {
    let own = OwnDatabase::new(server, name);
    if pg {
        on_server(own.connection(), "CREATE SCHEMA app");
    }
    let schema = if pg { "app" } else { "dbo" };
    let d = Demo::new(name);
    std::fs::write(
        d.dir.join("schema/product.yml"),
        format!(
            "table: {schema}.product\ncolumns:\n  id: {{type: int, nullable: false}}\n  recorded: {{type: int}}\n"
        ),
    )
    .unwrap();
    ok(d.run(&["plan", "--no-dev"]));
    d.commit();
    ok(d.run(&["bootstrap", "--db", own.connection()]));
    (own, d)
}

pub fn retained_column_ids(server: &str, pg: bool) {
    let (own, d) = column_fixture(server, pg, "history-retained2289");
    let connection = own.connection();
    let schema = if pg { "app" } else { "dbo" };
    on_server(
        connection,
        &format!("ALTER TABLE {schema}.product DROP COLUMN recorded"),
    );
    for recording in ["baseline", "snapshot"] {
        let args = if recording == "baseline" {
            vec![
                "baseline",
                "--reason",
                "accept the missing column",
                "--db",
                connection,
            ]
        } else {
            vec!["snapshot", "--force", "--db", connection]
        };
        ok(d.run(&args));
        let entries = records(connection, pg);
        let (id, text) = entries.last().unwrap();
        let snapshot = pbps_model::StateSnapshot::from_json(text).unwrap();
        let table: pbps_model::TableName = format!("{schema}.product").parse().unwrap();
        assert!(
            !snapshot.schema.tables[&table]
                .columns
                .contains_key("recorded")
        );
        assert!(
            snapshot
                .ids
                .columns
                .values()
                .any(|c| c == &table.column("recorded"))
        );
        let id = id.to_string();
        let before = project_bytes(&d.dir);
        let shown =
            json(ok(d.run(&[
                "state", "show", &id, "--db", connection, "--format", "json",
            ])));
        assert_eq!(
            shown["data"]["entry"]["snapshot"],
            serde_json::to_value(&snapshot).unwrap()
        );
        let compared = json(ok(d.run(&[
            "state", "diff", &id, &id, "--db", connection, "--format", "json",
        ])));
        assert!(
            compared["data"]["changes"]["changes"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        assert_eq!(project_bytes(&d.dir), before);
        let ids = std::fs::read(d.dir.join("schema.ids.json")).unwrap();
        ok(d.run(&["state", "export", &id, "--db", connection, "--force"]));
        let loaded = pbps_load::load_schema_dir(&d.dir.join("schema")).unwrap();
        assert!(
            !loaded.schema.tables[&table]
                .columns
                .contains_key("recorded")
        );
        assert_eq!(std::fs::read(d.dir.join("schema.ids.json")).unwrap(), ids);
    }
}

pub fn unrecorded_column_ids(server: &str, pg: bool) {
    let (own, d) = column_fixture(server, pg, "history-unrecorded2289");
    let connection = own.connection();
    let schema = if pg { "app" } else { "dbo" };
    let first = records(connection, pg)[0].0.to_string();
    on_server(
        connection,
        &format!("ALTER TABLE {schema}.product ADD observed int NULL"),
    );
    ok(d.run(&[
        "baseline",
        "--reason",
        "accept the added column",
        "--db",
        connection,
    ]));
    let second = records(connection, pg).last().unwrap().0.to_string();
    let default_sql = if pg {
        format!("ALTER TABLE {schema}.product ALTER COLUMN observed SET DEFAULT 7")
    } else {
        format!("ALTER TABLE {schema}.product ADD CONSTRAINT df_observed DEFAULT (7) FOR observed")
    };
    on_server(connection, &default_sql);
    ok(d.run(&["snapshot", "--force", "--db", connection]));
    let entries = records(connection, pg);
    let third = entries.last().unwrap().0.to_string();
    let table: pbps_model::TableName = format!("{schema}.product").parse().unwrap();
    for (_, text) in entries.iter().skip(1) {
        let snapshot = pbps_model::StateSnapshot::from_json(text).unwrap();
        assert!(
            snapshot.schema.tables[&table]
                .columns
                .contains_key("observed")
        );
        assert!(
            !snapshot
                .ids
                .columns
                .values()
                .any(|c| c == &table.column("observed"))
        );
    }
    let before = project_bytes(&d.dir);
    for added in [&second, &third] {
        for (from, to, add) in [(&first, added, true), (added, &first, false)] {
            let compared = json(ok(d.run(&[
                "state", "diff", from, to, "--db", connection, "--format", "json",
            ])));
            let changes: pbps_model::ChangeSet =
                serde_json::from_value(compared["data"]["changes"].clone()).unwrap();
            assert_eq!(changes.changes.len(), 1, "{changes:?}");
            let change = &changes.changes[0].change;
            let expected = if add {
                matches!(change, pbps_model::Change::AddColumn { table: actual, name, .. }
                    if actual == &table && name.raw() == "observed")
            } else {
                matches!(change, pbps_model::Change::DropColumn { column, .. }
                    if column == &table.column("observed"))
            };
            assert!(expected, "{changes:?}");
        }
    }
    let changed = json(ok(d.run(&[
        "state", "diff", &second, &third, "--db", connection, "--format", "json",
    ])));
    let changes: pbps_model::ChangeSet =
        serde_json::from_value(changed["data"]["changes"].clone()).unwrap();
    assert_eq!(changes.changes.len(), 1, "{changes:?}");
    assert!(
        changes.changes.iter().any(|p| matches!(&p.change,
            pbps_model::Change::AlterColumnDefault { column, .. }
                if column == &table.column("observed")
        )),
        "{changes:?}"
    );
    assert_eq!(project_bytes(&d.dir), before);
    assert_eq!(records(connection, pg), entries);
}
