use super::{BIN, Demo, code, stderr, stdout};
use std::process::Command;

fn succeeds(d: &Demo, args: &[&str]) -> String {
    let out = d.run(args);
    assert_eq!(code(&out), 0, "{args:?}: {}{}", stdout(&out), stderr(&out));
    stdout(&out)
}

pub fn process_count() {
    let mut counts = Vec::new();
    for n in [1, 20] {
        let d = Demo::nested(&format!("git-batch-{n}"), "db/app");
        for i in 0..n {
            std::fs::write(
                d.dir.join(format!("schema/t_{i}.yml")),
                format!("table: dbo.t_{i}\ncolumns:\n  id: {{type: int}}\n"),
            )
            .unwrap();
        }
        succeeds(&d, &["plan", "--no-dev"]);
        d.commit();
        let mut current = Vec::new();
        for (label, args) in [
            ("plan", vec!["plan", "--check", "--format", "json"]),
            (
                "validate",
                vec!["validate", "--since", "HEAD", "--format", "json"],
            ),
        ] {
            let trace = d.root.join(format!("{label}.trace"));
            let out = Command::new(BIN)
                .arg("--project")
                .arg(&d.dir)
                .args(&args)
                .env("GIT_TRACE", &trace)
                .output()
                .unwrap();
            assert_eq!(code(&out), 0, "{}{}", stdout(&out), stderr(&out));
            if label == "plan" {
                let report: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
                assert_eq!(report["data"]["changes"], 0);
            }
            let trace = std::fs::read_to_string(trace).unwrap();
            let commands: Vec<_> = trace
                .lines()
                .filter_map(|line| line.split_once("built-in: git ").map(|(_, c)| c))
                .collect();
            assert_eq!(
                commands
                    .iter()
                    .filter(|c| c.starts_with("cat-file --batch"))
                    .count(),
                1,
                "{label}: {trace}"
            );
            assert!(
                commands.iter().filter(|c| c.starts_with("show ")).count() <= 4,
                "{label}: {trace}"
            );
            current.push(commands.len());
        }
        counts.push(current);
    }
    assert_eq!(
        counts[0], counts[1],
        "both readers use a constant number of Git processes"
    );
}

pub fn paths() {
    let d = Demo::nested("git-batch-paths", "nested project/db");
    let old = "schéma old ' space";
    std::fs::rename(d.dir.join("schema"), d.dir.join(old)).unwrap();
    std::fs::write(
        d.dir.join("pbps.yml"),
        format!("dialect: mssql\nschema_dir: \"{old}\"\nids_file: old.ids.json\n"),
    )
    .unwrap();
    let names = vec!["space ' one.yaml", "té.yml"];
    #[cfg(unix)]
    let names = {
        let mut names = names;
        names.push("quote\"tab\tline\n.yml");
        names
    };
    for (i, name) in names.iter().enumerate() {
        std::fs::write(
            d.dir.join(old).join(name),
            format!("table: dbo.t_{i}\ncolumns:\n  id: {{type: int}}\n"),
        )
        .unwrap();
    }
    succeeds(&d, &["plan", "--no-dev"]);
    d.commit();
    std::fs::rename(d.dir.join(old), d.dir.join("schema new")).unwrap();
    std::fs::rename(d.dir.join("old.ids.json"), d.dir.join("new.ids.json")).unwrap();
    std::fs::write(
        d.dir.join("pbps.yml"),
        "dialect: mssql\nschema_dir: schema new\nids_file: new.ids.json\n",
    )
    .unwrap();
    let report: serde_json::Value =
        serde_json::from_str(&succeeds(&d, &["plan", "--check", "--format", "json"])).unwrap();
    assert_eq!(report["data"]["changes"], 0);
    std::fs::write(
        d.dir.join("schema new").join(names[0]),
        "table: dbo.t_0\ncolumns:\n  id: {type: int}\n  extra: {type: int}\n",
    )
    .unwrap();
    let plan = succeeds(&d, &["plan", "--no-dev"]);
    assert!(plan.contains("add column extra"), "{plan}");
    assert!(!plan.contains("create table"), "{plan}");
    succeeds(&d, &["validate", "--since", "HEAD"]);
}

pub fn broken() {
    for case in ["malformed", "missing", "gitlink"] {
        let d = Demo::new(&format!("git-batch-broken-{case}"));
        let good = "table: dbo.t\ncolumns:\n  id: {type: int}\n";
        d.table(good);
        succeeds(&d, &["plan", "--no-dev"]);
        d.commit();
        let expected_path = match case {
            "gitlink" => "schema/link.yml",
            "malformed" => "dbo.t.yml",
            _ => "schema/dbo.t.yml",
        };
        let git_value = |arg: &str| {
            let out = Command::new("git")
                .arg("-C")
                .arg(&d.root)
                .args(["rev-parse", arg])
                .output()
                .unwrap();
            assert!(out.status.success());
            String::from_utf8(out.stdout).unwrap().trim().to_owned()
        };
        match case {
            "malformed" => {
                d.table("table: [broken\n");
                d.commit();
                d.table(good);
            }
            "missing" => {
                let oid = git_value("HEAD:schema/dbo.t.yml");
                std::fs::remove_file(d.root.join(".git/objects").join(&oid[..2]).join(&oid[2..]))
                    .unwrap();
            }
            "gitlink" => {
                let oid = git_value("HEAD");
                d.git(&[
                    "update-index",
                    "--add",
                    "--cacheinfo",
                    &format!("160000,{oid},schema/link.yml"),
                ]);
                d.git(&["commit", "-qm", "record a non-blob declaration"]);
            }
            _ => unreachable!(),
        }
        let ids = std::fs::read(d.ids_path()).unwrap();
        for args in [
            vec!["plan", "--no-dev"],
            vec!["validate", "--since", "HEAD"],
        ] {
            let out = d.run(&args);
            let message = format!("{}{}", stdout(&out), stderr(&out));
            let refused = if args[0] == "validate" { 2 } else { 1 };
            assert_eq!(code(&out), refused, "{case}: {message}");
            assert!(
                message.contains(expected_path) && message.contains("HEAD"),
                "{case}: {message}"
            );
            assert_eq!(std::fs::read(d.ids_path()).unwrap(), ids);
        }
    }
}
