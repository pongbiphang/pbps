//! Reaching a server is distinct from reading its ledger and declared rows.
use super::{BIN, Demo, OwnDatabase, code, on_server, stderr, stdout, try_on_server};
use std::process::{Command, Output};

fn ok(out: Output) {
    assert_eq!(code(&out), 0, "{}{}", stdout(&out), stderr(&out));
}

fn ledger(pg: bool) -> &'static str {
    if pg {
        "public.__pbps_state"
    } else {
        "dbo.__pbps_state"
    }
}

fn prepare(pg: bool, name: &str) -> Demo {
    let d = Demo::new(name);
    let dialect = if pg { "postgres" } else { "mssql" };
    let schema = if pg { "app" } else { "dbo" };
    std::fs::write(
        d.dir.join("pbps.yml"),
        format!("dialect: {dialect}\nenvironments:\n  audit:\n    url_env: PBPS_VERIFY_1804_DB\n"),
    )
    .unwrap();
    std::fs::write(d.dir.join(format!("schema/{schema}.product.yml")), format!(
        "table: {schema}.product\ncolumns:\n  code: {{type: varchar(20), nullable: false}}\n  label: {{type: varchar(40), nullable: false}}\nprimary_key: {{name: pk_product, columns: [code]}}\ndata:\n  mode: exact\n  rows:\n    TEA: {{label: Tea}}\n"
    )).unwrap();
    ok(d.run(&["plan", "--no-dev"]));
    d.commit();
    d
}

fn verify(d: &Demo, connection: &str, json: bool) -> Output {
    let mut cmd = Command::new(BIN);
    cmd.arg("--project")
        .arg(&d.dir)
        .args(["verify", "--env", "audit"])
        .env("PBPS_VERIFY_1804_DB", connection);
    if json {
        cmd.args(["--format", "json"]);
    }
    let ids = std::fs::read(d.dir.join("schema.ids.json")).unwrap();
    let out = cmd.output().unwrap();
    assert_eq!(std::fs::read(d.dir.join("schema.ids.json")).unwrap(), ids);
    out
}

fn refused(d: &Demo, connection: &str, cause: &str) -> String {
    let out = verify(d, connection, true);
    assert_eq!(code(&out), 1, "{}{}", stdout(&out), stderr(&out));
    // Parsing the entire stdout also rejects two envelopes or stray prose.
    let report: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(report["command"], "verify");
    assert_eq!(report["result"], "unanswerable");
    let findings = report["findings"].as_array().unwrap();
    assert_eq!(findings.len(), 1, "{report}");
    assert_eq!(findings[0]["severity"], "error");
    let message = findings[0]["message"].as_str().unwrap();
    assert!(
        message.starts_with("audit: ") && message.to_lowercase().contains(cause),
        "{report}"
    );
    let human = verify(d, connection, false);
    assert_eq!(code(&human), 1, "{}{}", stdout(&human), stderr(&human));
    assert!(human.stdout.is_empty(), "{}", stdout(&human));
    assert!(
        stderr(&human).to_lowercase().contains(cause),
        "{}",
        stderr(&human)
    );
    findings[0]["id"].as_str().unwrap().to_owned()
}

fn recorded(connection: &str, pg: bool) -> Option<String> {
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
                "SELECT state_json FROM {} ORDER BY id DESC",
                ledger(pg)
            ))
            .await
            .unwrap()
            .first()
            .map(|row| {
                row.try_get::<&str>("state_json")
                    .unwrap()
                    .unwrap()
                    .to_owned()
            })
        })
}

fn set_recorded(connection: &str, pg: bool, text: &str) {
    on_server(
        connection,
        &format!(
            "UPDATE {} SET state_json = '{}'",
            ledger(pg),
            text.replace('\'', "''")
        ),
    );
}

pub fn state_failures(server: &str, pg: bool) {
    let own = OwnDatabase::new(server, "verify_state1804");
    let connection = own.connection();
    let d = prepare(pg, "verify-state1804");
    let mut ids = vec![(
        refused(&d, connection, "__pbps_state"),
        "state.uninitialized",
    )];
    if pg {
        on_server(connection, "CREATE SCHEMA app");
    }
    ok(d.run(&["bootstrap", "--db", connection]));
    ok(verify(&d, connection, true));
    let original = recorded(connection, pg).unwrap();
    for damaged in ["{not json", "{\"version\":999}"] {
        set_recorded(connection, pg, damaged);
        ids.push((refused(&d, connection, "unreadable"), "state.unreadable"));
        assert_eq!(recorded(connection, pg).as_deref(), Some(damaged));
        set_recorded(connection, pg, &original);
        ok(verify(&d, connection, true));
    }
    on_server(connection, &format!("DELETE FROM {}", ledger(pg)));
    ids.push((refused(&d, connection, "no entries"), "state.no-entries"));
    assert_eq!(recorded(connection, pg), None);
    // Keep every before-fix observation, rather than stopping at the first ID.
    assert!(
        ids.iter().all(|(actual, expected)| actual == expected),
        "{ids:?}"
    );
}

struct Login {
    server: String,
    name: String,
    pg: bool,
}

impl Drop for Login {
    fn drop(&mut self) {
        if self.pg {
            let _ = try_on_server(&self.server, &format!("DROP ROLE {}", self.name));
        } else {
            super::data_work::drop_test_login(&self.server, &self.name);
        }
    }
}

fn credentials(connection: &str, name: &str, password: &str, pg: bool) -> String {
    // The live-fixture contract uses simple keyword/ADO strings. Replace both
    // credentials so the negative control cannot silently retain the admin.
    if pg {
        let base = connection
            .split_whitespace()
            .filter(|part| !part.starts_with("user=") && !part.starts_with("password="))
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

pub fn permission_and_connection_failures(server: &str, pg: bool) {
    // Declared before the database so unwinding drops database grants first.
    let login = Login {
        server: server.to_owned(),
        name: format!("pbps_verify1804_{}", std::process::id()),
        pg,
    };
    let own = OwnDatabase::new(server, "verify_permissions1804");
    let connection = own.connection();
    let d = prepare(pg, "verify-permissions1804");
    if pg {
        on_server(connection, "CREATE SCHEMA app");
    }
    ok(d.run(&["bootstrap", "--db", connection]));
    let original = recorded(connection, pg).unwrap();
    let password = "Verify1804!Control";
    let name = &login.name;
    let product = if pg { "app.product" } else { "dbo.product" };
    if pg {
        on_server(
            connection,
            &format!(
                "CREATE ROLE {name} LOGIN NOSUPERUSER PASSWORD '{password}'; GRANT USAGE ON SCHEMA app, public TO {name}; GRANT SELECT ON public.__pbps_state TO {name};"
            ),
        );
    } else {
        on_server(
            server,
            &format!("CREATE LOGIN {name} WITH PASSWORD = '{password}', CHECK_POLICY = OFF;"),
        );
        on_server(
            connection,
            &format!(
                "CREATE USER {name} FOR LOGIN {name}; GRANT VIEW DEFINITION TO {name}; GRANT SELECT ON dbo.__pbps_state TO {name};"
            ),
        );
    }
    let reader = credentials(connection, name, password, pg);
    on_server(&reader, "SELECT 1");
    on_server(&reader, &format!("SELECT id FROM {}", ledger(pg)));
    assert!(try_on_server(&reader, &format!("SELECT code FROM {product}")).is_err());
    let mut ids = vec![(refused(&d, &reader, "denied"), "environment.unreadable")];
    on_server(connection, &format!("GRANT SELECT ON {product} TO {name}"));
    ok(verify(&d, &reader, true));
    on_server(
        connection,
        &format!("REVOKE SELECT ON {} FROM {name}", ledger(pg)),
    );
    ids.push((refused(&d, &reader, "denied"), "state.unreadable"));
    on_server(
        connection,
        &format!("GRANT SELECT ON {} TO {name}", ledger(pg)),
    );
    ok(verify(&d, &reader, true));
    let bad_password = credentials(connection, name, "Wrong1804!Password", pg);
    ids.push((
        refused(&d, &bad_password, if pg { "password" } else { "login" }),
        "environment.unreachable",
    ));
    ok(verify(&d, &reader, true));
    assert_eq!(recorded(connection, pg).as_deref(), Some(original.as_str()));
    assert!(
        ids.iter().all(|(actual, expected)| actual == expected),
        "{ids:?}"
    );
}
