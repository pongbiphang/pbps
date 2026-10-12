use super::{BIN, Demo, ONE_COLUMN, code, stderr, stdout};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn run(d: &Demo, args: &[&str]) -> Output {
    Command::new(BIN)
        .current_dir(&d.dir)
        .arg("--project")
        .arg(&d.dir)
        .args(args)
        .output()
        .unwrap()
}

fn success(d: &Demo, args: &[&str]) -> Output {
    let out = run(d, args);
    assert_eq!(code(&out), 0, "{args:?}: {}{}", stdout(&out), stderr(&out));
    out
}

fn project(name: &str, dialect: &str, mint_ids: bool) -> Demo {
    let d = Demo::nested(&format!("plan-outputs-{name}-{dialect}"), "project");
    std::fs::write(d.dir.join("pbps.yml"), format!("dialect: {dialect}\n")).unwrap();
    d.table(ONE_COLUMN);
    if mint_ids {
        success(&d, &["plan", "--no-dev"]);
    }
    d.commit();
    // A successful offline plan would mint an identity. Refusal has to precede
    // that write as well as both artifact writes.
    d.table(&format!("{ONE_COLUMN}  pending: {{type: int}}\n"));
    d
}

fn files(dir: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    let mut found = BTreeMap::new();
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.file_name().unwrap() == ".git" {
            continue;
        }
        if path.is_dir() {
            found.extend(files(&path));
        } else {
            found.insert(path.clone(), std::fs::read(path).unwrap());
        }
    }
    found
}

fn refused(d: &Demo, args: &[&str], location: Option<&Path>) {
    let before = files(&d.dir);
    let out = run(d, args);
    assert_eq!(code(&out), 1, "{args:?}: {}{}", stdout(&out), stderr(&out));
    // Parsing one value also refuses a duplicated JSON envelope from the
    // connected command's outer operational-error wrapper.
    let report: serde_json::Value = serde_json::from_str(&stdout(&out)).unwrap();
    assert_eq!(report["command"], "plan", "{report}");
    assert_eq!(report["result"], "unanswerable", "{report}");
    assert_eq!(report["findings"][0]["id"], "flags.conflicting", "{report}");
    if let Some(path) = location {
        assert_eq!(
            report["findings"][0]["location"]["file"],
            path.to_str().unwrap()
        );
    }
    assert_eq!(
        files(&d.dir),
        before,
        "{args:?}: refusal changed project files"
    );
}

#[test]
fn equivalent_artifact_destinations_refuse_before_any_identity_or_artifact_write() {
    for dialect in ["mssql", "postgres"] {
        let d = project("equivalent", dialect, true);
        std::fs::create_dir(d.dir.join("nested")).unwrap();
        std::fs::write(d.dir.join("both.out"), "already reviewed artifact\n").unwrap();
        for (out, sql) in [
            ("both.out", "both.out"),
            ("both.out", "./both.out"),
            ("both.out", "nested/../both.out"),
            ("new.out", "./new.out"),
        ] {
            refused(
                &d,
                &[
                    "plan", "--no-dev", "--out", out, "--sql", sql, "--format", "json",
                ],
                None,
            );
        }
    }
}

#[test]
fn project_inputs_are_never_plan_or_sql_destinations() {
    for dialect in ["mssql", "postgres"] {
        let d = project("inputs", dialect, true);
        std::fs::rename(d.dir.join("schema"), d.dir.join("declarations")).unwrap();
        std::fs::rename(d.ids_path(), d.dir.join("identities.json")).unwrap();
        std::fs::write(
            d.dir.join("pbps.yml"),
            format!("dialect: {dialect}\nschema_dir: declarations\nids_file: identities.json\n"),
        )
        .unwrap();
        std::fs::create_dir(d.dir.join("declarations/roles")).unwrap();
        std::fs::write(
            d.dir.join("declarations/roles/reader.yaml"),
            "role: reader\n",
        )
        .unwrap();
        std::fs::write(d.dir.join("other.out"), "other reviewed artifact\n").unwrap();
        for input in [
            "pbps.yml",
            "identities.json",
            "declarations/dbo.t.yml",
            "declarations/roles/reader.yaml",
        ] {
            for (out, sql) in [(input, "other.out"), ("other.out", input)] {
                refused(
                    &d,
                    &[
                        "plan", "--no-dev", "--out", out, "--sql", sql, "--format", "json",
                    ],
                    Some(&d.dir.join(input)),
                );
            }
        }
    }
}

#[test]
fn an_absent_identity_file_cannot_be_replaced_by_an_artifact() {
    for dialect in ["mssql", "postgres"] {
        let d = project("absent-ids", dialect, false);
        for flag in ["--out", "--sql"] {
            refused(
                &d,
                &[
                    "plan",
                    "--no-dev",
                    flag,
                    "schema.ids.json",
                    "--format",
                    "json",
                ],
                Some(&d.ids_path()),
            );
            assert!(!d.ids_path().exists());
        }
    }
}

#[test]
fn connected_planning_refuses_output_aliases_before_contacting_the_target() {
    for (dialect, target) in [
        (
            "mssql",
            "Server=127.0.0.1,1;Database=unused;User Id=unused;Password=unused;TrustServerCertificate=true;Connection Timeout=1",
        ),
        (
            "postgres",
            "postgres://unused:unused@127.0.0.1:1/unused?connect_timeout=1",
        ),
    ] {
        let d = project("connected", dialect, true);
        d.table(ONE_COLUMN);
        refused(
            &d,
            &[
                "plan",
                "--db",
                target,
                "--out",
                "both.out",
                "--sql",
                "./both.out",
                "--format",
                "json",
            ],
            None,
        );
        refused(
            &d,
            &[
                "plan",
                "--db",
                target,
                "--out",
                "schema.ids.json",
                "--format",
                "json",
            ],
            Some(&d.ids_path()),
        );
    }
}

#[test]
fn snapshot_and_configured_resolver_baselines_are_protected_inputs() {
    for dialect in ["mssql", "postgres"] {
        let d = project("baselines", dialect, true);
        let ids: serde_json::Value =
            serde_json::from_slice(&std::fs::read(d.ids_path()).unwrap()).unwrap();
        let baseline = serde_json::json!({
            "version": pbps_model::state::OLDEST_READABLE_VERSION,
            "kind": "baseline", "operator": "offline-test", "ids": ids,
            "schema": {"tables": {"dbo.t": {"columns": {"id": {"type": "bigint", "nullable": false}}}}},
        });
        std::fs::write(d.dir.join("baseline.json"), baseline.to_string()).unwrap();
        for flag in ["--out", "--sql"] {
            refused(
                &d,
                &[
                    "plan",
                    "--base",
                    "baseline.json",
                    "--no-dev",
                    flag,
                    "baseline.json",
                    "--format",
                    "json",
                ],
                Some(Path::new("baseline.json")),
            );
        }
        std::fs::create_dir(d.dir.join("seeds")).unwrap();
        std::fs::write(
            d.dir.join("seeds/server.sql"),
            "CREATE SCHEMA external_server;\n",
        )
        .unwrap();
        std::fs::write(
            d.dir.join("seeds/docker.sql"),
            "CREATE SCHEMA external_docker;\n",
        )
        .unwrap();
        std::fs::write(d.dir.join("pbps.yml"), format!(
            "dialect: {dialect}\nresolvers:\n  server: {{kind: server, url_env: UNUSED_RESOLVER, baseline: seeds/server.sql}}\n  docker: {{kind: docker, image: postgres:18, baseline: seeds/docker.sql}}\n"
        )).unwrap();
        for input in ["seeds/server.sql", "seeds/docker.sql"] {
            for flag in ["--out", "--sql"] {
                refused(
                    &d,
                    &["plan", "--no-dev", flag, input, "--format", "json"],
                    Some(&d.dir.join(input)),
                );
            }
        }
    }
}

#[cfg(unix)]
#[test]
fn symlink_and_hardlink_aliases_protect_inputs_and_artifacts() {
    let d = project("filesystem-aliases", "postgres", true);
    std::os::unix::fs::symlink(d.ids_path(), d.dir.join("ids-alias.json")).unwrap();
    std::fs::hard_link(
        d.dir.join("schema/dbo.t.yml"),
        d.dir.join("declaration-alias.out"),
    )
    .unwrap();
    for (alias, input) in [
        ("ids-alias.json", d.ids_path()),
        ("declaration-alias.out", d.dir.join("schema/dbo.t.yml")),
    ] {
        for flag in ["--out", "--sql"] {
            refused(
                &d,
                &["plan", "--no-dev", flag, alias, "--format", "json"],
                Some(&input),
            );
        }
    }
    std::fs::write(d.dir.join("reviewed.out"), "reviewed artifact\n").unwrap();
    std::fs::hard_link(d.dir.join("reviewed.out"), d.dir.join("reviewed-alias.out")).unwrap();
    refused(
        &d,
        &[
            "plan",
            "--no-dev",
            "--out",
            "reviewed.out",
            "--sql",
            "reviewed-alias.out",
            "--format",
            "json",
        ],
        None,
    );
}

#[test]
fn distinct_outputs_keep_overwrite_behavior_and_explain_reads_the_preview() {
    for dialect in ["mssql", "postgres"] {
        let d = project("distinct", dialect, true);
        for file in ["reviewed.json", "reviewed.sql"] {
            std::fs::write(d.dir.join(file), "same old artifact bytes\n").unwrap();
        }
        success(
            &d,
            &[
                "plan",
                "--no-dev",
                "--out",
                "reviewed.json",
                "--sql",
                "reviewed.sql",
                "--format",
                "json",
            ],
        );
        success(
            &d,
            &["explain", "--plan", "reviewed.json", "--format", "json"],
        );
        let sql = std::fs::read_to_string(d.dir.join("reviewed.sql")).unwrap();
        assert!(sql.contains("pending"), "{sql}");
        let before = files(&d.dir);
        for flag in ["--out", "--sql"] {
            let out = run(
                &d,
                &[
                    "plan",
                    "--no-dev",
                    flag,
                    "missing/parent/artifact.out",
                    "--format",
                    "json",
                ],
            );
            assert_eq!(code(&out), 1, "{}{}", stdout(&out), stderr(&out));
            let report: serde_json::Value = serde_json::from_str(&stdout(&out)).unwrap();
            assert_eq!(report["findings"][0]["id"], "plan.unwritable", "{report}");
            assert_eq!(files(&d.dir), before);
        }
    }
}
