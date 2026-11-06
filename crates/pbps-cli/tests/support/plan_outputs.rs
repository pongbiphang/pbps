use super::{BIN, Demo, ONE_COLUMN, code, stderr, stdout};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn command(d: &Demo, args: &[&str]) -> Command {
    let mut command = Command::new(BIN);
    command
        .current_dir(&d.dir)
        .arg("--project")
        .arg(&d.dir)
        .args(args);
    command
}

fn run(d: &Demo, args: &[&str]) -> Output {
    command(d, args).output().unwrap()
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
    refused_command(d, args, location, &mut command(d, args));
}

fn refused_command(d: &Demo, args: &[&str], location: Option<&Path>, command: &mut Command) {
    let before = files(&d.dir);
    let out = command.output().unwrap();
    assert_eq!(code(&out), 1, "{args:?}: {}{}", stdout(&out), stderr(&out));
    // Parsing one value also refuses a duplicated JSON envelope from the
    // connected command's outer operational-error wrapper.
    let report: serde_json::Value = serde_json::from_str(&stdout(&out)).unwrap();
    assert_eq!(report["command"], "plan", "{report}");
    assert_eq!(report["result"], "unanswerable", "{report}");
    assert_eq!(report["findings"][0]["id"], "flags.conflicting", "{report}");
    if let Some(path) = location {
        // Directory enumeration uses native separators; CLI inputs may use
        // either separator on Windows. Compare the path, not its spelling.
        assert_eq!(
            Path::new(report["findings"][0]["location"]["file"].as_str().unwrap()),
            path
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
        std::fs::rename(crate::idtree::dir(&d.dir), d.dir.join("identities")).unwrap();
        std::fs::write(
            d.dir.join("pbps.yml"),
            format!("dialect: {dialect}\nschema_dir: declarations\nids_dir: identities\n"),
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
            "identities/dbo.t.json",
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
                Some(&d.dir.join("schema.ids.json")),
            );
            assert!(!crate::idtree::exists(&d.dir));
        }
    }
}

#[test]
fn configured_fingerprint_keys_are_inputs_even_without_selecting_their_environment() {
    for dialect in ["mssql", "postgres"] {
        let d = project("fingerprint-keys", dialect, true);
        let absolute = d.dir.join("spare.key");
        for path in [d.dir.join("target.key"), absolute.clone()] {
            std::fs::write(path, "opaque key bytes that planning must preserve\n").unwrap();
        }
        std::fs::write(
            d.dir.join("pbps.yml"),
            format!(
                "dialect: {dialect}\nenvironments:\n  dev:\n    url_env: PBPS_1800_UNUSED_DB\n    fingerprint_key_file: target.key\n  spare:\n    url_env: PBPS_1800_UNUSED_DB\n    fingerprint_key_file: {}\n",
                serde_json::to_string(&absolute).unwrap(),
            ),
        )
        .unwrap();
        for input in ["target.key", "spare.key"] {
            for flag in ["--out", "--sql"] {
                for mode in [vec!["plan", "--no-dev"], vec!["plan", "--env", "dev"]] {
                    let mut args = mode;
                    args.extend([flag, input, "--format", "json"]);
                    refused(&d, &args, Some(&d.dir.join(input)));
                }
            }
        }
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink("target.key", d.dir.join("key-link")).unwrap();
            std::fs::hard_link(&absolute, d.dir.join("key-hardlink")).unwrap();
            for (alias, input) in [("key-link", "target.key"), ("key-hardlink", "spare.key")] {
                for flag in ["--out", "--sql"] {
                    refused(
                        &d,
                        &["plan", "--no-dev", flag, alias, "--format", "json"],
                        Some(&d.dir.join(input)),
                    );
                }
            }
        }
        // Offline preview does not need to parse or use any environment's
        // key; distinct artifacts must still be allowed beside opaque bytes.
        success(
            &d,
            &[
                "plan",
                "--no-dev",
                "--out",
                "preview.json",
                "--sql",
                "preview.sql",
            ],
        );
        success(&d, &["explain", "--plan", "preview.json"]);
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
            Some(&d.dir.join("schema.ids.json")),
        );
    }
}

const OFFLINE_TARGETS: [(&str, &str); 2] = [
    (
        "mssql",
        "Server=127.0.0.1,1;Database=unused;User Id=unused;Password=unused;TrustServerCertificate=true;Connection Timeout=1",
    ),
    (
        "postgres",
        "postgres://unused:unused@127.0.0.1:1/unused?connect_timeout=1",
    ),
];

fn assertion_alias_is_refused(name: &str, flag: &str, alias: fn(&Path) -> PathBuf) {
    for (dialect, target) in OFFLINE_TARGETS {
        let d = project(name, dialect, true);
        d.table(ONE_COLUMN);
        std::fs::create_dir(d.dir.join("checks")).unwrap();
        std::fs::write(d.dir.join("checks/first.sql"), "SELECT 2 WHERE 1 = 0;\n").unwrap();
        let input = d.dir.join("checks/x.sql");
        std::fs::write(&input, "SELECT 1 WHERE 1 = 0;\n").unwrap();
        let destination = alias(&input);
        let args = [
            "plan",
            "--db",
            target,
            "--assert",
            "checks/first.sql",
            "--assert",
            "checks/x.sql",
            flag,
            destination.to_str().unwrap(),
            "--format",
            "json",
        ];
        // Supply the assertion login so an unguarded invocation reaches the
        // unreachable target instead of stopping on a missing variable.
        refused_command(
            &d,
            &args,
            Some(&input.canonicalize().unwrap()),
            command(&d, &args).env("PBPS_ASSERTION_CONN", target),
        );
    }
}

#[test]
fn an_assertion_is_never_a_plan_destination() {
    assertion_alias_is_refused("assert-out", "--out", Path::to_owned);
}

#[test]
fn an_assertion_is_never_a_sql_destination() {
    assertion_alias_is_refused("assert-sql", "--sql", Path::to_owned);
}

#[cfg(unix)]
fn assertion_symlink(input: &Path) -> PathBuf {
    let alias = input.with_file_name("symlink.sql");
    std::os::unix::fs::symlink(input, &alias).unwrap();
    alias
}

#[cfg(unix)]
#[test]
fn an_assertion_symlink_is_never_a_plan_destination() {
    assertion_alias_is_refused("assert-link-out", "--out", assertion_symlink);
}

#[cfg(unix)]
#[test]
fn an_assertion_symlink_is_never_a_sql_destination() {
    assertion_alias_is_refused("assert-link-sql", "--sql", assertion_symlink);
}

#[cfg(unix)]
fn assertion_hardlink(input: &Path) -> PathBuf {
    let alias = input.with_file_name("hardlink.sql");
    std::fs::hard_link(input, &alias).unwrap();
    alias
}

#[cfg(unix)]
#[test]
fn an_assertion_hardlink_is_never_a_plan_destination() {
    assertion_alias_is_refused("assert-hardlink-out", "--out", assertion_hardlink);
}

#[cfg(unix)]
#[test]
fn an_assertion_hardlink_is_never_a_sql_destination() {
    assertion_alias_is_refused("assert-hardlink-sql", "--sql", assertion_hardlink);
}

#[test]
fn assertion_outputs_can_be_distinct_files_with_the_same_bytes() {
    for (dialect, target) in OFFLINE_TARGETS {
        let d = project("assert-distinct", dialect, true);
        d.table(ONE_COLUMN);
        for path in ["x.sql", "artifact.json", "artifact.sql"] {
            std::fs::write(d.dir.join(path), "SELECT 1 WHERE 1 = 0;\n").unwrap();
        }
        let before = files(&d.dir);
        let out = command(
            &d,
            &[
                "plan",
                "--db",
                target,
                "--assert",
                "x.sql",
                "--out",
                "artifact.json",
                "--sql",
                "artifact.sql",
                "--format",
                "json",
            ],
        )
        .env("PBPS_ASSERTION_CONN", target)
        .output()
        .unwrap();
        assert_eq!(code(&out), 1, "{}{}", stdout(&out), stderr(&out));
        let report: serde_json::Value = serde_json::from_str(&stdout(&out)).unwrap();
        assert_eq!(report["findings"][0]["id"], "plan.failed", "{report}");
        assert_eq!(files(&d.dir), before);
    }
}

#[test]
fn assertion_aliases_follow_the_reader_when_the_working_directory_differs_from_the_project() {
    for (dialect, target) in OFFLINE_TARGETS {
        let d = project("assert-cwd", dialect, true);
        d.table(ONE_COLUMN);
        let cwd = d.dir.join("invocation");
        for directory in [d.dir.join("checks"), cwd.join("checks")] {
            std::fs::create_dir_all(&directory).unwrap();
            std::fs::write(directory.join("x.sql"), "SELECT 1 WHERE 1 = 0;\n").unwrap();
        }
        let input = cwd.join("checks/x.sql").canonicalize().unwrap();
        for flag in ["--out", "--sql"] {
            let args = [
                "plan",
                "--db",
                target,
                "--assert",
                "checks/x.sql",
                flag,
                "checks/x.sql",
                "--format",
                "json",
            ];
            refused_command(
                &d,
                &args,
                Some(&input),
                command(&d, &args)
                    .current_dir(&cwd)
                    .env("PBPS_ASSERTION_CONN", target),
            );
            // The same spelling from the project root names a different file.
            // Protecting that file would refuse an ordinary distinct output.
            let before = files(&d.dir);
            let out = command(
                &d,
                &[
                    "plan",
                    "--db",
                    target,
                    "--assert",
                    "checks/x.sql",
                    flag,
                    "../checks/x.sql",
                    "--format",
                    "json",
                ],
            )
            .current_dir(&cwd)
            .env("PBPS_ASSERTION_CONN", target)
            .output()
            .unwrap();
            assert_eq!(code(&out), 1, "{}{}", stdout(&out), stderr(&out));
            let report: serde_json::Value = serde_json::from_str(&stdout(&out)).unwrap();
            assert_eq!(report["findings"][0]["id"], "plan.failed", "{report}");
            assert_eq!(files(&d.dir), before);
        }
    }
}

#[test]
fn snapshot_baselines_and_resolver_externals_files_are_protected_inputs() {
    for dialect in ["mssql", "postgres"] {
        let d = project("baselines", dialect, true);
        let ids: serde_json::Value = serde_json::to_value(crate::idtree::model(&d.dir)).unwrap();
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
            "dialect: {dialect}\nresolvers:\n  server: {{kind: server, url_env: UNUSED_RESOLVER, externals: seeds/server.sql}}\n  docker: {{kind: docker, image: postgres:18, externals: seeds/docker.sql}}\n"
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
    let ids_file = crate::idtree::dir(&d.dir).join("dbo.t.json");
    std::os::unix::fs::symlink(&ids_file, d.dir.join("ids-alias.json")).unwrap();
    std::fs::hard_link(
        d.dir.join("schema/dbo.t.yml"),
        d.dir.join("declaration-alias.out"),
    )
    .unwrap();
    for (alias, input) in [
        ("ids-alias.json", ids_file),
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
