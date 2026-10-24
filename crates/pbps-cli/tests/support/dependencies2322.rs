use super::*;
use pbps_model::dependency::Comparison;

fn success(output: Output) -> Output {
    assert_eq!(code(&output), 0, "{}{}", stdout(&output), stderr(&output));
    output
}

fn declaration(database: &str, object: &str, kind: &str, columns: &str) -> String {
    format!("dependency: {database}.dbo.{object}\nkind: {kind}\ncolumns:\n{columns}")
}

const COLUMNS: &str = "  id: {type: int, nullable: false}\n  name: {type: nvarchar(100)}\n";

fn write(demo: &Demo, text: &str) {
    std::fs::write(demo.dir.join("schema/dependency.yml"), text).unwrap();
}

fn initialized(server: &str, slug: &str) -> (Demo, OwnDatabase) {
    let target = OwnDatabase::new(server, slug);
    let demo = Demo::new(slug);
    demo.table(ONE_COLUMN);
    success(demo.run(&["plan", "--no-dev"]));
    demo.commit();
    success(demo.run(&["bootstrap", "--db", target.connection()]));
    (demo, target)
}

fn planned(demo: &Demo, connection: &str, json: bool) -> (Output, PathBuf) {
    let path = demo.dir.join("dependencies-plan.json");
    let mut args = vec!["plan", "--db", connection, "--out", path.to_str().unwrap()];
    if json {
        args.extend(["--format", "json"]);
    }
    (demo.run(&args), path)
}

fn comparison(path: &std::path::Path) -> Comparison {
    let plan: pbps_model::SavedPlan =
        serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    plan.dependencies.values().next().unwrap().comparison
}

fn assert_reports(demo: &Demo, connection: &str, expected: Comparison) -> PathBuf {
    let (output, path) = planned(demo, connection, false);
    let status_line = format!("): {}\n", expected.as_str());
    assert!(stdout(&success(output)).contains(&status_line));
    let output = success(demo.run(&["explain", "--plan", path.to_str().unwrap()]));
    assert!(
        stdout(&output).contains(&status_line),
        "{}",
        stdout(&output)
    );
    assert!(stdout(&output).contains("nvarchar(100)"));
    let (output, _) = planned(demo, connection, true);
    let output = success(output);
    let validator = jsonschema::validator_for(&envelope_schema()).unwrap();
    envelope_matches_schema(&validator, "plan", &output);
    let output: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let data = &output["data"]["dependencies"];
    let item = data.as_object().unwrap().values().next().unwrap();
    assert_eq!(item["comparison"], expected.as_str());
    assert_eq!(item["declaration"]["kind"], "table");
    let output = success(demo.run(&[
        "explain",
        "--plan",
        path.to_str().unwrap(),
        "--format",
        "json",
    ]));
    envelope_matches_schema(&validator, "explain", &output);
    let output: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(output["data"]["dependencies"], *data);
    path
}

#[test]
fn declarations_stay_outside_ids_and_invalid_dialects_and_types_are_named() {
    let demo = Demo::new("dependencies2322_loader");
    demo.table(ONE_COLUMN);
    write(
        &demo,
        &declaration("otherdb", "customers", "table", COLUMNS),
    );
    success(demo.run(&["plan", "--no-dev"]));
    let ids: pbps_model::IdsFile =
        serde_json::from_slice(&std::fs::read(demo.ids_path()).unwrap()).unwrap();
    assert_eq!(ids.tables.len(), 1);
    assert!(
        !std::fs::read_to_string(demo.ids_path())
            .unwrap()
            .contains("otherdb")
    );
    success(demo.run(&["fmt"]));
    success(demo.run(&["fmt", "--check"]));
    for (dialect, columns, expected) in [
        ("postgres", "  id: {type: int}\n", "SQL Server"),
        ("mssql", "  id: {type: nvarchar(4001)}\n", "4001"),
        ("mssql", "  id: {type: jsonb}\n", "jsonb"),
    ] {
        std::fs::write(demo.dir.join("pbps.yml"), format!("dialect: {dialect}\n")).unwrap();
        write(
            &demo,
            &declaration("otherdb", "customers", "table", columns),
        );
        let output = demo.run(&["validate"]);
        assert_ne!(code(&output), 0);
        let message = format!("{}{}", stdout(&output), stderr(&output));
        assert!(message.contains("otherdb.dbo.customers"), "{message}");
        assert!(message.contains(expected), "{message}");
    }
}

fn assert_offline_dependency_refusal(slug: &str, dialect: &str, columns: &str) {
    // Current identities keep --check from hiding acceptance behind identity.stale.
    for current_ids in [false, true] {
        let demo = Demo::new(&format!("{slug}_{current_ids}"));
        demo.table(ONE_COLUMN);
        if current_ids {
            success(demo.run(&["plan", "--no-dev"]));
        }
        let before = current_ids.then(|| std::fs::read(demo.ids_path()).unwrap());
        std::fs::write(demo.dir.join("pbps.yml"), format!("dialect: {dialect}\n")).unwrap();
        write(
            &demo,
            &declaration("otherdb", "customers", "table", columns),
        );
        let validated = demo.run(&["validate", "--format", "json"]);
        assert_eq!(code(&validated), 2, "{}", stderr(&validated));
        let validated: serde_json::Value = serde_json::from_slice(&validated.stdout).unwrap();
        let expected: Vec<_> = validated["findings"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|finding| finding["severity"] == "error")
            .cloned()
            .collect();
        assert_eq!(expected.len(), 1);
        assert_eq!(expected[0]["id"], "dialect.rejected");
        assert!(
            expected[0]["message"]
                .as_str()
                .unwrap()
                .contains("otherdb.dbo.customers")
        );
        let validated_human = demo.run(&["validate"]);
        assert_eq!(code(&validated_human), 2);
        let plan_path = demo.dir.join("preview.json");
        let sql_path = demo.dir.join("preview.sql");
        for mode in ["--no-dev", "--check"] {
            for json in [false, true] {
                let mut args = vec!["plan", mode];
                if mode == "--no-dev" {
                    // --check refuses artifact flags before loading declarations.
                    args.extend([
                        "--out",
                        plan_path.to_str().unwrap(),
                        "--sql",
                        sql_path.to_str().unwrap(),
                    ]);
                }
                if json {
                    args.extend(["--format", "json"]);
                }
                let output = demo.run(&args);
                assert_eq!(
                    code(&output),
                    2,
                    "{args:?}: {}{}",
                    stdout(&output),
                    stderr(&output)
                );
                if json {
                    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
                    assert_eq!(report["command"], "plan");
                    assert_eq!(report["result"], validated["result"]);
                    assert_eq!(report["findings"], serde_json::json!(expected));
                    assert!(report["data"].is_null());
                } else {
                    assert_eq!(stderr(&output), stderr(&validated_human));
                    assert!(output.stdout.is_empty());
                }
                assert!(!plan_path.exists());
                assert!(!sql_path.exists());
                match &before {
                    Some(bytes) => assert_eq!(std::fs::read(demo.ids_path()).unwrap(), *bytes),
                    None => assert!(!demo.ids_path().exists()),
                }
            }
        }
    }
}

#[test]
fn offline_plans_refuse_foreign_dependency_dialects_before_any_output() {
    assert_offline_dependency_refusal("dependencies2322_offline_dialect", "postgres", COLUMNS);
}

#[test]
fn offline_plans_refuse_invalid_dependency_types_before_any_output() {
    assert_offline_dependency_refusal(
        "dependencies2322_offline_type",
        "mssql",
        "  id: {type: jsonb}\n",
    );
}

#[test]
fn offline_plans_keep_valid_dependencies_as_uncompared_previews() {
    let demo = Demo::new("dependencies2322_offline_valid");
    demo.table(ONE_COLUMN);
    write(
        &demo,
        &declaration("otherdb", "customers", "table", COLUMNS),
    );
    let plan_path = demo.dir.join("preview.json");
    let sql_path = demo.dir.join("preview.sql");
    let output = success(demo.run(&[
        "plan",
        "--no-dev",
        "--out",
        plan_path.to_str().unwrap(),
        "--sql",
        sql_path.to_str().unwrap(),
        "--format",
        "json",
    ]));
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let plan: pbps_model::SavedPlan =
        serde_json::from_slice(&std::fs::read(&plan_path).unwrap()).unwrap();
    assert_eq!(plan.origin, pbps_model::PlanOrigin::Preview);
    assert_eq!(plan.dependencies.len(), 1);
    assert_eq!(comparison(&plan_path), Comparison::NotCompared);
    assert_eq!(
        report["data"]["dependencies"],
        serde_json::to_value(&plan.dependencies).unwrap()
    );
    assert!(
        std::fs::read_to_string(&sql_path)
            .unwrap()
            .contains("CREATE TABLE [dbo].[t]")
    );
    let before = std::fs::read(demo.ids_path()).unwrap();
    for args in [
        vec!["plan", "--check"],
        vec!["plan", "--check", "--format", "json"],
    ] {
        success(demo.run(&args));
        assert_eq!(std::fs::read(demo.ids_path()).unwrap(), before);
    }
}

#[test]
#[ignore = "needs live SQL Server"]
fn readable_dependencies_compare_exact_shapes_and_name_each_refusal() {
    let server = std::env::var("PBPS_TEST_DB").unwrap();
    let (demo, target) = initialized(&server, "dependencies2322_readable");
    let other = OwnDatabase::new(&server, "dependencies2322_other");
    on_server(
        other.connection(),
        "CREATE TABLE dbo.customers (id int NOT NULL, name nvarchar(100) NULL);",
    );
    on_server(
        other.connection(),
        "CREATE VIEW dbo.customer_view AS SELECT id, name FROM dbo.customers;",
    );
    write(
        &demo,
        &declaration(other.name(), "customers", "table", COLUMNS),
    );
    let path = assert_reports(&demo, target.connection(), Comparison::Compared);
    let approved = plan_checksum(&path);
    let original = std::fs::read(&path).unwrap();
    for field in ["declaration", "comparison"] {
        let mut plan: serde_json::Value = serde_json::from_slice(&original).unwrap();
        let dependency = plan["dependencies"]
            .as_object_mut()
            .unwrap()
            .values_mut()
            .next()
            .unwrap();
        if field == "declaration" {
            dependency["declaration"]["columns"]["id"]["nullable"] = true.into();
        } else {
            dependency["comparison"] = "declared, not compared".into();
        }
        std::fs::write(&path, serde_json::to_vec(&plan).unwrap()).unwrap();
        let output = demo.run(&[
            "apply",
            "--db",
            target.connection(),
            "--plan",
            path.to_str().unwrap(),
            "--checksum",
            &approved,
        ]);
        assert_ne!(code(&output), 0);
        assert!(format!("{}{}", stdout(&output), stderr(&output)).contains("checksum"));
    }
    std::fs::write(&path, original).unwrap();
    for (database, object, kind, columns, expected) in [
        (
            other.name(),
            "customers",
            "table",
            "  id: {type: bigint, nullable: false}\n  name: {type: nvarchar(100)}\n",
            "type differs",
        ),
        (
            other.name(),
            "customers",
            "table",
            "  id: {type: int, nullable: false}\n  name: {type: nvarchar(99)}\n",
            "type differs",
        ),
        (
            other.name(),
            "customers",
            "table",
            "  id: {type: int, nullable: false}\n  name: {type: nvarchar(100)}\n  absent: {type: int}\n",
            "absent",
        ),
        (
            other.name(),
            "customers",
            "table",
            "  id: {type: int, nullable: false}\n",
            "extra catalog column",
        ),
        (
            other.name(),
            "customers",
            "table",
            "  id: {type: int}\n  name: {type: nvarchar(100)}\n",
            "nullability",
        ),
        (
            other.name(),
            "missing",
            "table",
            COLUMNS,
            "object is missing",
        ),
        (
            other.name(),
            "customer_view",
            "table",
            COLUMNS,
            "kind differs",
        ),
        (
            target.name(),
            "t",
            "table",
            "  id: {type: bigint, nullable: false}\n",
            "target",
        ),
        (
            "pbps2322_nonexistent_database",
            "customers",
            "table",
            COLUMNS,
            "database is missing",
        ),
    ] {
        write(&demo, &declaration(database, object, kind, columns));
        let (output, _) = planned(&demo, target.connection(), false);
        assert_ne!(code(&output), 0);
        let message = format!("{}{}", stdout(&output), stderr(&output));
        assert!(
            message.contains(&format!("{database}.dbo.{object}")),
            "{message}"
        );
        assert!(message.contains(expected), "{message}");
    }
    write(
        &demo,
        &declaration(other.name(), "customer_view", "view", COLUMNS),
    );
    let (output, path) = planned(&demo, target.connection(), true);
    success(output);
    assert_eq!(comparison(&path), Comparison::Compared);
    on_server(
        other.connection(),
        "CREATE TABLE dbo.scalars (amount decimal(19,4) NULL, ts datetime2(3) NULL, bytes varbinary(max) NULL, text nchar(7) NULL);",
    );
    let scalars = "  amount: {type: 'decimal(19,4)'}\n  ts: {type: datetime2(3)}\n  bytes: {type: varbinary(max)}\n  text: {type: nchar(7)}\n";
    write(
        &demo,
        &declaration(other.name(), "scalars", "table", scalars),
    );
    success(planned(&demo, target.connection(), false).0);
    for changed in [
        scalars.replace("19,4", "18,4"),
        scalars.replace("19,4", "19,3"),
        scalars.replace("datetime2(3)", "datetime2(4)"),
    ] {
        write(
            &demo,
            &declaration(other.name(), "scalars", "table", &changed),
        );
        let (output, _) = planned(&demo, target.connection(), false);
        assert_ne!(code(&output), 0);
        assert!(
            stderr(&output).contains("type differs"),
            "{}",
            stderr(&output)
        );
    }
}

struct Login {
    server: String,
    name: String,
}
impl Login {
    fn create(server: &str, target: &OwnDatabase) -> (Self, String) {
        let name = format!("pbps2322_login_{}", target.name());
        on_server(
            server,
            &format!("CREATE LOGIN [{name}] WITH PASSWORD = 'Pbps!Test2322', CHECK_POLICY = OFF;"),
        );
        on_server(
            target.connection(),
            &format!(
                "CREATE USER [{name}] FOR LOGIN [{name}]; ALTER ROLE db_owner ADD MEMBER [{name}];"
            ),
        );
        let connection = with_key(
            &with_key(target.connection(), "User Id", &name),
            "Password",
            "Pbps!Test2322",
        );
        (
            Self {
                server: server.into(),
                name,
            },
            connection,
        )
    }
}
impl Drop for Login {
    fn drop(&mut self) {
        after_test_on_server(&self.server, &format!("DROP LOGIN [{}];", self.name));
    }
}

#[test]
#[ignore = "needs live SQL Server"]
fn unreadable_dependencies_are_reported_without_claiming_comparison() {
    let server = std::env::var("PBPS_TEST_DB").unwrap();
    let (demo, target) = initialized(&server, "dependencies2322_unreadable");
    let other = OwnDatabase::new(&server, "dependencies2322_hidden");
    on_server(
        other.connection(),
        "CREATE TABLE dbo.customers (id int NOT NULL, name nvarchar(100) NULL);",
    );
    let (login, connection) = Login::create(&server, &target);
    write(
        &demo,
        &declaration(other.name(), "customers", "table", COLUMNS),
    );
    assert_reports(&demo, &connection, Comparison::NotCompared);
    on_server(
        &server,
        &format!("DENY VIEW ANY DATABASE TO [{}];", login.name),
    );
    assert_reports(&demo, &connection, Comparison::NotCompared);
    write(
        &demo,
        &declaration(
            "pbps2322_nonexistent_database",
            "customers",
            "table",
            COLUMNS,
        ),
    );
    let (output, _) = planned(&demo, &connection, false);
    assert_ne!(code(&output), 0);
    assert!(
        stderr(&output).contains("database is missing"),
        "{}",
        stderr(&output)
    );
    write(
        &demo,
        &declaration(other.name(), "customers", "table", COLUMNS),
    );
    on_server(
        &server,
        &format!("REVOKE VIEW ANY DATABASE FROM [{}];", login.name),
    );
    on_server(
        other.connection(),
        &format!(
            "CREATE USER [{0}] FOR LOGIN [{0}]; GRANT CONNECT TO [{0}];",
            login.name
        ),
    );
    assert_reports(&demo, &connection, Comparison::NotCompared);
    on_server(
        other.connection(),
        &format!(
            "GRANT VIEW DEFINITION TO [{0}]; DENY VIEW DEFINITION ON OBJECT::dbo.customers TO [{0}];",
            login.name
        ),
    );
    assert_reports(&demo, &connection, Comparison::NotCompared);
    on_server(
        other.connection(),
        &format!(
            "REVOKE VIEW DEFINITION ON OBJECT::dbo.customers FROM [{}];",
            login.name
        ),
    );
    assert_reports(&demo, &connection, Comparison::Compared);
    // Absence in this now-readable catalog must refuse, not become an assumption.
    write(
        &demo,
        &declaration(other.name(), "missing", "table", COLUMNS),
    );
    let (output, _) = planned(&demo, &connection, false);
    assert_ne!(code(&output), 0);
    assert!(
        stderr(&output).contains("object is missing"),
        "{}",
        stderr(&output)
    );
}

#[test]
#[ignore = "needs live SQL Server"]
fn apply_binds_dependencies_transactionally_without_repeating_the_comparison() {
    let server = std::env::var("PBPS_TEST_DB").unwrap();
    for missing in [false, true] {
        let slug = if missing {
            "dependencies2322_rollback"
        } else {
            "dependencies2322_apply"
        };
        let (demo, target) = initialized(&server, slug);
        let other = OwnDatabase::new(&server, &format!("{slug}_other"));
        on_server(
            other.connection(),
            "CREATE TABLE dbo.customers (id int NOT NULL, name nvarchar(100) NULL);",
        );
        let (login, planner) = Login::create(&server, &target);
        let column = if missing { "missing" } else { "id" };
        let columns = if missing {
            "  missing: {type: int}\n"
        } else {
            COLUMNS
        };
        write(
            &demo,
            &declaration(other.name(), "customers", "table", columns),
        );
        std::fs::write(
            demo.dir.join("schema/first.yml"),
            "table: dbo.created_in_transaction\ncolumns: {id: {type: int}}\n",
        )
        .unwrap();
        std::fs::write(demo.dir.join("schema/view.yml"), format!("view: dbo.dependency_view\ndefinition: |-\n  SELECT {column} FROM [{}].dbo.customers\n", other.name())).unwrap();
        success(demo.run(&["plan", "--no-dev"]));
        demo.commit();
        let (output, path) = planned(&demo, &planner, false);
        success(output);
        assert_eq!(comparison(&path), Comparison::NotCompared);
        // A fresh comparison would now reject the extra column. Apply must
        // leave binding to the engine, without re-reading this dependency.
        on_server(
            other.connection(),
            "ALTER TABLE dbo.customers ADD added_after_plan int NULL;",
        );
        let output = demo.run(&[
            "apply",
            "--db",
            target.connection(),
            "--plan",
            path.to_str().unwrap(),
            "--checksum",
            &plan_checksum(&path),
        ]);
        if missing {
            assert_ne!(code(&output), 0);
            assert!(format!("{}{}", stdout(&output), stderr(&output)).contains("missing"));
            on_server(
                target.connection(),
                "IF OBJECT_ID('dbo.created_in_transaction') IS NOT NULL OR OBJECT_ID('dbo.dependency_view') IS NOT NULL THROW 51000, 'dependency apply did not roll back', 1;",
            );
        } else {
            success(output);
            on_server(
                target.connection(),
                "IF OBJECT_ID('dbo.dependency_view') IS NULL THROW 51000, 'dependency view was not created', 1;",
            );
            success(demo.run(&["verify", "--db", target.connection()]));
        }
        drop(login);
    }
}
