use super::*;
use pbps_model::resolver::{ObjectIdentity, ObjectOwnership, PlanAnalysis, Surface};

#[test]
#[ignore = "needs PostgreSQL 18; set PBPS_TEST_PG_DB"]
fn a_populated_partition_detach_seals_its_carried_columns_on_pg18() {
    check(&server());
}

#[test]
#[ignore = "needs PostgreSQL 16; set PBPS_TEST_PG_OLD_DB"]
fn a_populated_partition_detach_seals_its_carried_columns_on_pg16() {
    check(&std::env::var("PBPS_TEST_PG_OLD_DB").unwrap());
}

fn check(server: &str) {
    let own = OwnDatabase::new(server, "detach1566");
    let connection = own.connection();
    on_server(connection, "CREATE SCHEMA app");
    let scratch = Confined::new(server);
    let d = Demo::new("detach1566");
    let columns = "columns:\n  id: {type: integer, nullable: false}\n  payload: {type: text}\n";
    let parent = |parts: &str| {
        format!("table: app.events\n{columns}partition_by: [id]\npartitions:\n{parts}")
    };
    write_declarations(
        &d,
        &[
            (
                "app.events.yml",
                &parent(
                    "  history: {from: [\"0\"], to: [\"10\"]}\n  current: {from: [\"10\"], to: [\"20\"]}\n",
                ),
            ),
            (
                "app.f%28numeric%29.function.yml",
                "function: app.f(numeric)\npublic_execute: true\ndefinition: |-\n  (numeric) RETURNS numeric LANGUAGE sql IMMUTABLE RETURN $1\n",
            ),
            (
                "app.v.view.yml",
                "view: app.v\ndefinition: SELECT app.f(1) AS n\n",
            ),
        ],
    );
    successful(run(&d, &["plan"], None));
    d.commit();
    successful(run(&d, &["bootstrap", "--db", connection], None));
    on_server(
        connection,
        "INSERT INTO app.events VALUES (1, 'kept'), (11, 'current')",
    );
    write_declarations(
        &d,
        &[
            (
                "app.events.yml",
                &parent("  current: {from: [\"10\"], to: [\"20\"]}\n"),
            ),
            ("app.history.yml", &format!("table: app.history\n{columns}")),
            (
                "app.f%28integer%29.function.yml",
                "function: app.f(integer)\npublic_execute: true\ndefinition: |-\n  (integer) RETURNS numeric LANGUAGE sql IMMUTABLE RETURN $1\n",
            ),
        ],
    );
    successful(run(&d, &["plan"], None));
    d.commit();
    // The ordinary path establishes that this fixture describes a legal detach.
    let ordinary = d.dir.join("ordinary.json");
    successful(run(
        &d,
        &[
            "plan",
            "--db",
            connection,
            "--out",
            ordinary.to_str().unwrap(),
        ],
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
    let targets = Targets {
        target: connection,
        scratch: &scratch.connection,
    };
    let artifact = d.dir.join("resolved.json");
    let sql = d.dir.join("resolved.sql");
    let published = run(&d, &plan_args(&artifact, &sql), Some(&targets));
    assert_eq!(
        code(&published),
        0,
        "a partition detach must seal its carried columns: {}{}",
        stdout(&published),
        stderr(&published)
    );
    let saved: pbps_model::SavedPlan =
        serde_json::from_slice(&std::fs::read(&artifact).unwrap()).unwrap();
    let PlanAnalysis::Resolved(evidence) = &saved.analysis else {
        panic!("the plan must actually use the resolver")
    };
    let leaf = pbps_model::TableName::new("app", "history");
    let transition = evidence
        .transitions()
        .iter()
        .find(|t| t.surface == Surface::Table(leaf.clone()))
        .expect("detach transition");
    for name in ["id", "payload"] {
        let object = ObjectIdentity {
            class: "column".into(),
            name: vec![name.into()],
            signature: vec![ObjectIdentity {
                class: "pg_class".into(),
                name: vec!["app".into(), "history".into()],
                signature: vec![],
            }],
        };
        assert!(
            transition.before.contains(&object) && transition.after.contains(&object),
            "inherited {name} is carried across detach"
        );
        let opening = evidence
            .before()
            .prerequisites()
            .iter()
            .find(|p| p.object == object)
            .unwrap();
        assert_eq!(
            opening.ownership,
            ObjectOwnership::Surface(Surface::Table(leaf.clone()))
        );
    }
    // Inventory authority is not granted by a closing label alone: a column
    // already captured at opening cannot be relabelled as a fresh installation.
    let mut malformed: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&artifact).unwrap()).unwrap();
    for item in malformed["analysis"]["evidence"]["transitions"]
        .as_array_mut()
        .unwrap()
    {
        item["before"].as_array_mut().unwrap().retain(|o| {
            !(o["class"] == "column"
                && o["name"] == serde_json::json!(["id"])
                && o["signature"][0]["name"] == serde_json::json!(["app", "history"]))
        });
    }
    let bad = d.dir.join("invalid-install.json");
    std::fs::write(&bad, serde_json::to_vec_pretty(&malformed).unwrap()).unwrap();
    let refused = run(
        &d,
        &[
            "explain",
            "--plan",
            bad.to_str().unwrap(),
            "--format",
            "json",
        ],
        None,
    );
    assert_ne!(
        code(&refused),
        0,
        "an opening column cannot become an unaccounted installation"
    );

    let checksum = plan_checksum(&artifact);
    successful(
        Command::new(BIN)
            .arg("--project")
            .arg(&d.dir)
            .args([
                "apply",
                "--env",
                "prod",
                "--plan",
                artifact.to_str().unwrap(),
                "--checksum",
                &checksum,
                "--allow",
                "destructive,constraint",
            ])
            .env("PBPS_TARGET_1516", connection)
            .env("PBPS_SCRATCH_1516", "host=127.0.0.1 port=1 dbname=unused")
            .output()
            .unwrap(),
    );
    assert_eq!(
        scalar(
            connection,
            "SELECT count(*) FROM app.history WHERE id=1 AND payload='kept'"
        ),
        1
    );
    assert_eq!(
        scalar(
            connection,
            "SELECT count(*) FROM app.events WHERE id=11 AND payload='current'"
        ),
        1
    );
    assert_eq!(
        scalar(
            connection,
            "SELECT count(*) FROM pg_inherits WHERE inhrelid='app.history'::regclass"
        ),
        0
    );
    successful(run(&d, &["verify", "--env", "prod"], Some(&targets)));
}
