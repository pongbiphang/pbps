//! A bounded cross-version witness, sharing the CLI projects of the full suites.
use super::{Demo, code, plan_checksum, stderr, stdout};
use pbps_db::{Conn, Driver};
use serde_json::Value;
use std::process::Output;

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

fn text(driver: Driver, connection: &str, sql: &str) -> String {
    runtime().block_on(async {
        let mut conn = Conn::connect(driver, connection).await.unwrap();
        conn.query(sql).await.unwrap()[0]
            .try_get_at::<&str>(0)
            .unwrap()
            .unwrap()
            .to_owned()
    })
}

fn execute(driver: Driver, connection: &str, sql: &str) -> Result<(), pbps_db::DbError> {
    runtime().block_on(async {
        Conn::connect(driver, connection)
            .await?
            .execute(sql)
            .await?;
        Ok(())
    })
}

fn ok(output: Output) -> Output {
    assert_eq!(code(&output), 0, "{}{}", stdout(&output), stderr(&output));
    output
}

fn entries(demo: &Demo, connection: &str) -> Value {
    let output = ok(demo.run(&["state", "list", "--db", connection, "--format", "json"]));
    serde_json::from_str::<Value>(&stdout(&output)).unwrap()["data"]["entries"].clone()
}

struct Fixture {
    driver: Driver,
    server: String,
    database: String,
    role: String,
    postgres: bool,
    cleaned: bool,
}

impl Fixture {
    fn cleanup(&mut self) -> Result<(), pbps_db::DbError> {
        let sql = if self.postgres {
            format!("DROP DATABASE IF EXISTS {} WITH (FORCE)", self.database)
        } else {
            format!(
                "IF DB_ID('{}') IS NOT NULL BEGIN ALTER DATABASE [{}] SET SINGLE_USER WITH ROLLBACK IMMEDIATE; DROP DATABASE [{}]; END",
                self.database, self.database, self.database
            )
        };
        execute(self.driver, &self.server, &sql)?;
        if self.postgres {
            execute(
                self.driver,
                &self.server,
                &format!("DROP ROLE IF EXISTS {}", self.role),
            )?;
        }
        self.cleaned = true;
        Ok(())
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if !self.cleaned {
            // Explicit cleanup below is asserted; this path also covers partial setup.
            if let Err(error) = self.cleanup() {
                eprintln!("compatibility fixture cleanup failed: {error}");
            }
        }
    }
}

pub fn run(driver: Driver, variable: &str, default_cell: &str) {
    let server = std::env::var(variable).expect(variable);
    let cell = std::env::var("PBPS_COMPAT_CELL").unwrap_or_else(|_| default_cell.into());
    let matrix: Value = serde_json::from_str(include_str!(
        "../../../../scripts/compatibility-matrix.json"
    ))
    .unwrap();
    let row = &matrix[&cell];
    let postgres = matches!(driver, Driver::Postgres);
    assert_eq!(row["engine"], if postgres { "postgres" } else { "mssql" });
    // Read through the very DSN used by the CLI, before creating any object.
    // Docker metadata alone cannot establish which server that endpoint reaches.
    let observed = if postgres {
        text(
            driver,
            &server,
            "SELECT current_setting('server_version_num')",
        )
    } else {
        text(
            driver,
            &server,
            "SELECT CONVERT(varchar(128),SERVERPROPERTY('ProductVersion')) + '|' + CONVERT(varchar(128),SERVERPROPERTY('Edition')) + '|' + CONVERT(varchar(10),SERVERPROPERTY('EngineEdition')) + '|' + CONVERT(varchar(128),SERVERPROPERTY('Collation'))",
        )
    };
    let expected = if postgres {
        row["version"].as_str().unwrap().to_owned()
    } else {
        format!(
            "{}|{}|{}|{}",
            row["version"].as_str().unwrap(),
            row["edition"].as_str().unwrap(),
            row["engine_edition"],
            row["collation"].as_str().unwrap()
        )
    };
    assert_eq!(
        observed, expected,
        "endpoint is not the requested compatibility cell"
    );
    eprintln!(
        "compatibility cell={cell} actual={observed} image={}",
        row["image"]
    );

    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let database = format!("pbps_compat_{}_{nonce}", std::process::id());
    let role = format!("pc_{}_{nonce}", std::process::id());
    // Register ownership before the first CREATE, including the cluster-level role.
    let mut fixture = Fixture {
        driver,
        server: server.clone(),
        database: database.clone(),
        role: role.clone(),
        postgres,
        cleaned: false,
    };
    execute(driver, &server, &format!("CREATE DATABASE {database}")).unwrap();
    let connection = if postgres {
        let parts: Vec<_> = server
            .split_whitespace()
            .filter(|part| !part.starts_with("dbname="))
            .collect();
        format!("{} dbname={database}", parts.join(" "))
    } else {
        format!("{server};Database={database}")
    };
    let schema = if postgres { "app" } else { "dbo" };
    if postgres {
        execute(driver, &connection, "CREATE SCHEMA app").unwrap();
        execute(driver, &server, &format!("CREATE ROLE {role}")).unwrap();
    }
    let d = Demo::new(&format!("compatibility-{cell}"));
    let table = format!(
        "table: {schema}.t\ncolumns:\n  id: {{type: bigint, nullable: false}}\n  amount: {{type: 'numeric(12,2)'}}\nprimary_key: [id]\n"
    );
    d.table(&table);
    let role_file = d.dir.join("schema/reader.yml");
    let grants = |permissions: &str| {
        std::fs::write(
            &role_file,
            format!(
                "role: {role}\ngrants:\n  {schema}.t: [{permissions}]\n{}",
                if postgres {
                    "  schema::app: [usage]\n"
                } else {
                    ""
                }
            ),
        )
        .unwrap();
    };
    grants("select");
    ok(d.run(&["plan"]));
    d.commit();
    ok(d.run(&["bootstrap", "--db", &connection]));
    ok(d.run(&["verify", "--db", &connection]));
    let initial = entries(&d, &connection);
    assert_eq!(initial.as_array().unwrap().len(), 1);
    execute(
        driver,
        &connection,
        &format!("INSERT INTO {schema}.t (id, amount) VALUES (1, 12.34)"),
    )
    .unwrap();
    assert!(
        execute(
            driver,
            &connection,
            &format!("INSERT INTO {schema}.t (id) VALUES (1)")
        )
        .is_err(),
        "primary key must reject a duplicate"
    );
    if postgres {
        execute(
            driver,
            &connection,
            &format!("SET ROLE {role}; SELECT * FROM {schema}.t"),
        )
        .unwrap();
        assert_eq!(
            text(
                driver,
                &connection,
                &format!("SELECT has_table_privilege('{role}', '{schema}.t', 'SELECT')::text")
            ),
            "true"
        );
    } else {
        assert_eq!(
            text(
                driver,
                &connection,
                &format!(
                    "SELECT CONVERT(varchar(10), COUNT(*)) FROM sys.database_permissions p JOIN sys.database_principals r ON r.principal_id=p.grantee_principal_id WHERE r.name='{role}' AND p.permission_name='SELECT' AND p.major_id=OBJECT_ID('{schema}.t')"
                )
            ),
            "1"
        );
    }

    let changed = table.replace("primary_key:", "  label: {type: varchar(40)}\nprimary_key:");
    d.table(&changed);
    ok(d.run(&["plan"]));
    d.commit();
    let plan = d.dir.join("compatibility.json");
    let path = plan.to_str().unwrap();
    ok(d.run(&["plan", "--db", &connection, "--out", path]));
    let checksum = plan_checksum(&plan);
    let bad = d.run(&[
        "apply",
        "--db",
        &connection,
        "--plan",
        path,
        "--checksum",
        "wrong",
    ]);
    assert_ne!(code(&bad), 0);
    assert!(stderr(&bad).contains("checksum"), "{}", stderr(&bad));
    assert_eq!(entries(&d, &connection), initial);

    let rt = runtime();
    let mut held = rt.block_on(Conn::connect(driver, &connection)).unwrap();
    rt.block_on(async {
        if postgres {
            pbps_pg::state::lock(&mut held, "compatibility-holder")
                .await
                .unwrap();
        } else {
            pbps_mssql::state::lock(&mut held, "compatibility-holder")
                .await
                .unwrap();
        }
    });
    let apply = || {
        d.run(&[
            "apply",
            "--db",
            &connection,
            "--plan",
            path,
            "--checksum",
            &checksum,
        ])
    };
    let denied = apply();
    assert_eq!(code(&denied), 1, "{}{}", stdout(&denied), stderr(&denied));
    assert!(
        stderr(&denied).contains("compatibility-holder"),
        "{}",
        stderr(&denied)
    );
    assert_eq!(entries(&d, &connection), initial);
    ok(d.run(&["unlock", "--db", &connection]));
    drop(held);
    ok(apply());
    ok(d.run(&["verify", "--db", &connection]));
    let applied = entries(&d, &connection);
    assert_eq!(applied.as_array().unwrap().len(), 2);
    let pulled = Demo::new(&format!("compatibility-pull-{cell}"));
    ok(pulled.run(&["pull", "--db", &connection]));
    ok(pulled.run(&["verify", "--db", &connection]));
    execute(
        driver,
        &connection,
        &format!("UPDATE {schema}.t SET label='kept' WHERE id=1"),
    )
    .unwrap();
    assert_eq!(
        text(
            driver,
            &connection,
            &format!("SELECT label FROM {schema}.t WHERE id=1")
        ),
        "kept"
    );

    // One real feature boundary per dialect, with positive execution on capable cells.
    let capable = if postgres {
        row["version"] == "180006"
    } else {
        row["engine_edition"] == 3
    };
    if postgres {
        grants("select, maintain");
    } else {
        d.table(&format!(
            "{changed}indexes:\n  ix_amount:\n    columns: [amount]\nstrategy:\n  online: true\n"
        ));
    }
    ok(d.run(&["plan"]));
    d.commit();
    let feature = d.dir.join("feature.json");
    let feature_path = feature.to_str().unwrap();
    std::fs::write(&feature, "sentinel").unwrap();
    let output = d.run(&[
        "plan",
        "--db",
        &connection,
        "--out",
        feature_path,
        "--format",
        "json",
    ]);
    if capable {
        ok(output);
        ok(d.run(&[
            "apply",
            "--db",
            &connection,
            "--plan",
            feature_path,
            "--checksum",
            &plan_checksum(&feature),
        ]));
        ok(d.run(&["verify", "--db", &connection]));
        assert_eq!(entries(&d, &connection).as_array().unwrap().len(), 3);
        if postgres {
            assert_eq!(
                text(
                    driver,
                    &connection,
                    &format!(
                        "SELECT has_table_privilege('{role}', '{schema}.t', 'MAINTAIN')::text"
                    )
                ),
                "true"
            );
        }
    } else {
        assert_eq!(
            code(&output),
            if postgres { 1 } else { 2 },
            "{}{}",
            stdout(&output),
            stderr(&output)
        );
        assert!(
            stdout(&output).contains(if postgres {
                "permission_support"
            } else {
                "plan.online-unsupported"
            }),
            "{}",
            stdout(&output)
        );
        assert_eq!(std::fs::read_to_string(&feature).unwrap(), "sentinel");
        assert_eq!(entries(&d, &connection), applied);
    }
    fixture.cleanup().unwrap();
    let remaining = if postgres {
        format!(
            "SELECT (SELECT count(*) FROM pg_database WHERE datname='{database}')::text || ':' || (SELECT count(*) FROM pg_roles WHERE rolname='{role}')::text"
        )
    } else {
        format!("SELECT CASE WHEN DB_ID('{database}') IS NULL THEN '0:0' ELSE '1:0' END")
    };
    assert_eq!(
        text(driver, &server, &remaining),
        "0:0",
        "owned objects survived cleanup"
    );
}
