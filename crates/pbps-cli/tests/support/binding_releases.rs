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
