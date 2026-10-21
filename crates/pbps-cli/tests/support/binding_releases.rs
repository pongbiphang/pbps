//! A desired definition can release a binding that its baseline still holds.

use super::{Demo, OwnDatabase, code, on_server, stderr, stdout, try_on_server};

pub fn check() {
    let server = std::env::var("PBPS_TEST_DB").expect("set PBPS_TEST_DB");
    for (variant, options, body, result) in [
        ("unbound", "", "dbo.f(@x)", 3),
        ("retarget", "WITH SCHEMABINDING", "dbo.h(@x)", 12),
        ("removed", "WITH SCHEMABINDING", "@x", 2),
    ] {
        let slug = format!("binding_release_{variant}");
        let own = OwnDatabase::new(&server, &slug);
        let connection = own.connection();
        let demo = Demo::new(&slug);
        let success = |out: std::process::Output| {
            assert_eq!(code(&out), 0, "{}{}", stdout(&out), stderr(&out));
        };
        let definition = |options: &str, body: &str| {
            format!("(@x int) RETURNS int {options} AS BEGIN RETURN {body} END")
        };
        let write = |name: &str, definition: &str| {
            demo.module(
                &format!("{name}.yml"),
                &format!("function: dbo.{name}\ndefinition: |-\n  {definition}\n"),
            );
        };
        for (name, body) in [("f", "@x"), ("g", "dbo.f(@x)"), ("h", "@x + 10")] {
            write(name, &definition("WITH SCHEMABINDING", body));
        }
        success(demo.run(&["plan"]));
        demo.commit();
        success(demo.run(&["bootstrap", "--db", connection]));
        let new_f = definition("WITH SCHEMABINDING", "@x + 1");
        // This is the engine failure the baseline requirement must prevent,
        // before the ordinary planner is allowed to repair that same order.
        let refused = try_on_server(connection, &format!("ALTER FUNCTION dbo.f {new_f}"))
            .expect_err("the old schema-bound caller must hold its callee");
        assert!(
            refused.contains("3729") && refused.contains("g"),
            "{refused}"
        );
        write("f", &new_f);
        write("g", &definition(options, body));
        let preview = demo.dir.join("preview.sql");
        success(demo.run(&["plan", "--no-dev", "--sql", preview.to_str().unwrap()]));
        let script = std::fs::read_to_string(&preview).unwrap();
        let g = script.find("FUNCTION [dbo].[g]").expect(&script);
        let f = script.find("FUNCTION [dbo].[f]").expect(&script);
        assert!(
            g < f,
            "the offline shared schedule must release first: {script}"
        );
        demo.commit();
        let path = demo.dir.join("binding-plan.json");
        success(demo.run(&["plan", "--db", connection, "--out", path.to_str().unwrap()]));
        let saved: pbps_model::SavedPlan =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        let rewritten: Vec<_> = saved
            .changes
            .changes
            .iter()
            .filter_map(|step| {
                if let pbps_model::Change::AlterModule { id, .. } = &step.change {
                    Some(id.referenced_name().unwrap().name.clone())
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(rewritten, ["g", "f"], "{:?}", saved.changes);
        let allowed = saved
            .changes
            .gated_risks()
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(",");
        success(demo.run(&[
            "apply",
            "--db",
            connection,
            "--plan",
            path.to_str().unwrap(),
            "--checksum",
            &saved.checksum(),
            "--allow",
            &allowed,
        ]));
        on_server(
            connection,
            &format!(
                "IF dbo.f(2) <> 3 OR dbo.g(2) <> {result} THROW 51000, 'wrong rewritten result', 1;"
            ),
        );
        success(demo.run(&["verify", "--db", connection]));
        success(demo.run(&["plan", "--db", connection, "--out", path.to_str().unwrap()]));
        let next: pbps_model::SavedPlan =
            serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        assert!(next.changes.is_empty(), "{:?}", next.changes);
    }
}

pub fn check_independent_connected_release() {
    let server = std::env::var("PBPS_TEST_DB").expect("set PBPS_TEST_DB");
    let own = OwnDatabase::new(&server, "binding_computed2228");
    let connection = own.connection();
    for (name, body) in [("f", "@x"), ("g", "dbo.f(@x)"), ("z", "@x")] {
        on_server(
            connection,
            &format!(
                "CREATE FUNCTION dbo.{name} (@x int) RETURNS int WITH SCHEMABINDING AS BEGIN RETURN {body} END"
            ),
        );
    }
    on_server(
        connection,
        "CREATE TABLE dbo.t (id int, c AS dbo.z(id)); INSERT INTO dbo.t (id) VALUES (2)",
    );
    let demo = Demo::new("binding-computed2228");
    let success =
        |out: std::process::Output| assert_eq!(code(&out), 0, "{}{}", stdout(&out), stderr(&out));
    success(demo.run(&["pull", "--db", connection]));
    demo.commit();
    success(demo.run(&[
        "baseline",
        "--db",
        connection,
        "--reason",
        "adopt combined fixture",
    ]));
    let mut found = 0;
    for path in super::walk(&demo.dir.join("schema")) {
        let text = std::fs::read_to_string(&path).unwrap();
        if text.contains("function: dbo.f") {
            std::fs::write(path, "function: dbo.f\ndefinition: |-\n  (@x int) RETURNS int WITH SCHEMABINDING AS BEGIN RETURN @x + 1 END\n").unwrap();
            found += 1;
        } else if text.contains("function: dbo.g") {
            std::fs::write(path, "function: dbo.g\ndefinition: |-\n  (@x int) RETURNS int AS BEGIN RETURN dbo.f(@x) END\n").unwrap();
            found += 1;
        } else if text.contains("function: dbo.z") {
            std::fs::remove_file(path).unwrap();
            found += 1;
        }
    }
    assert_eq!(found, 3);
    demo.table("table: dbo.t\ncolumns:\n  id: {type: int}\n");
    success(demo.run(&["plan", "--no-dev"]));
    demo.commit();
    let path = demo.dir.join("combined-plan.json");
    success(demo.run(&["plan", "--db", connection, "--out", path.to_str().unwrap()]));
    let saved: pbps_model::SavedPlan =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    let at = |name: &str| {
        saved
            .changes
            .changes
            .iter()
            .position(|p| p.change.subject() == name)
            .unwrap()
    };
    let drop_column = saved
        .changes
        .changes
        .iter()
        .position(|p| matches!(p.change, pbps_model::Change::DropComputedColumn { .. }))
        .unwrap();
    assert!(drop_column < at("dbo.z"), "{:?}", saved.changes);
    assert!(at("dbo.g") < at("dbo.f"), "{:?}", saved.changes);
    let allowed = saved
        .changes
        .gated_risks()
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(",");
    success(demo.run(&[
        "apply",
        "--db",
        connection,
        "--plan",
        path.to_str().unwrap(),
        "--checksum",
        &saved.checksum(),
        "--allow",
        &allowed,
    ]));
    on_server(
        connection,
        "IF dbo.f(2) <> 3 OR dbo.g(2) <> 3 OR OBJECT_ID(N'dbo.z') IS NOT NULL THROW 51000, 'wrong combined release', 1; IF EXISTS (SELECT 1 FROM sys.columns WHERE object_id = OBJECT_ID(N'dbo.t') AND name = N'c') THROW 51000, 'computed column still stands', 1;",
    );
    success(demo.run(&["verify", "--db", connection]));
    success(demo.run(&["plan", "--db", connection, "--out", path.to_str().unwrap()]));
    let next: pbps_model::SavedPlan =
        serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    assert!(next.changes.is_empty(), "{:?}", next.changes);
}
