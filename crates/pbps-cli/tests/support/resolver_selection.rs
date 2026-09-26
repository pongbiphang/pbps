use super::*;

fn configure(d: &Demo, dialect: &str) {
    std::fs::write(d.dir.join("pbps.yml"), format!(
        "dialect: {dialect}\nresolve_with: local\nresolvers:\n  local:\n    kind: docker\n    image: registry.invalid/team/engine:preloaded\n  internal:\n    kind: docker\n    image: registry.invalid/team/engine:approved\n    pull: if_missing\n  scratch:\n    kind: server\n    url_env: PBPS_RESOLVER_UNSET_606\nenvironments:\n  prod:\n    url_env: PBPS_TARGET_606\n    resolve_with: scratch\n  stage:\n    url_env: PBPS_TARGET_606\n"
    )).unwrap();
}

fn run(d: &Demo, args: &[&str], target: Option<&str>) -> Output {
    let mut cmd = Command::new(BIN);
    cmd.arg("--project")
        .arg(&d.dir)
        .args(args)
        .env_remove("PBPS_RESOLVER_UNSET_606");
    if let Some(target) = target {
        cmd.env("PBPS_TARGET_606", target);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let bin = d.dir.join("trap-bin");
        std::fs::create_dir_all(&bin).unwrap();
        let docker = bin.join("docker");
        std::fs::write(
            &docker,
            "#!/bin/sh\nprintf invoked > \"$PBPS_RESOLVER_DOCKER_MARKER\"\nexit 99\n",
        )
        .unwrap();
        std::fs::set_permissions(&docker, std::fs::Permissions::from_mode(0o755)).unwrap();
        let mut paths = vec![bin];
        paths.extend(std::env::split_paths(&std::env::var_os("PATH").unwrap()));
        cmd.env("PATH", std::env::join_paths(paths).unwrap());
        cmd.env("PBPS_RESOLVER_DOCKER_MARKER", d.dir.join("docker-invoked"));
    }
    let output = cmd.output().unwrap();
    assert!(
        !d.dir.join("docker-invoked").exists(),
        "invoked Docker: {args:?}"
    );
    output
}

fn successful(output: Output) -> Output {
    assert_eq!(code(&output), 0, "{}{}", stdout(&output), stderr(&output));
    output
}

fn report(output: Output) -> serde_json::Value {
    let value: serde_json::Value = serde_json::from_str(&stdout(&output)).unwrap();
    let schema: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../schemas/envelope.schema.json"),
        )
        .unwrap(),
    )
    .unwrap();
    assert!(
        jsonschema::validator_for(&schema).unwrap().is_valid(&value),
        "{value}"
    );
    crate::envelope_archives::assert_accepted_by_archives(&value, "resolver selection");
    value
}

pub fn offline(dialect: &str) {
    let d = Demo::new("resolver606offline");
    d.table(ONE_COLUMN);
    configure(&d, dialect);
    let preview = d.dir.join("preview.json");
    successful(run(&d, &["plan", "--out", preview.to_str().unwrap()], None));
    d.commit();
    let check = report(successful(run(
        &d,
        &["plan", "--check", "--format", "json"],
        None,
    )));
    assert!(check["data"].get("resolver_selection").is_none(), "{check}");
    successful(run(
        &d,
        &["explain", "--plan", preview.to_str().unwrap()],
        None,
    ));
    // Even an unresolved default is unused by the offline workflow.
    let path = d.dir.join("pbps.yml");
    let config = std::fs::read_to_string(&path)
        .unwrap()
        .replace("resolve_with: local", "resolve_with: missing");
    std::fs::write(path, config).unwrap();
    successful(run(&d, &["plan", "--check"], None));
    let saved = std::fs::read(&preview).unwrap();
    for args in [
        vec![
            "plan",
            "--resolve-with",
            "local",
            "--out",
            preview.to_str().unwrap(),
            "--format",
            "json",
        ],
        vec![
            "plan",
            "--check",
            "--resolve-with",
            "local",
            "--format",
            "json",
        ],
        vec![
            "plan",
            "--dev",
            "docker://unused",
            "--resolve-with",
            "local",
            "--format",
            "json",
        ],
        vec![
            "plan",
            "--db",
            "unreachable",
            "--check",
            "--resolve-with",
            "local",
            "--format",
            "json",
        ],
        vec![
            "plan",
            "--db",
            "unreachable",
            "--dev",
            "docker://unused",
            "--resolve-with",
            "local",
            "--format",
            "json",
        ],
    ] {
        let output = run(&d, &args, None);
        assert_eq!(code(&output), 1, "{}{}", stdout(&output), stderr(&output));
        let value = report(output);
        assert_eq!(value["findings"][0]["id"], "flags.conflicting", "{value}");
        assert_eq!(std::fs::read(&preview).unwrap(), saved);
    }
    let unknown = run(
        &d,
        &[
            "plan",
            "--db",
            "unreachable",
            "--resolve-with",
            "typo",
            "--out",
            preview.to_str().unwrap(),
            "--format",
            "json",
        ],
        None,
    );
    assert_eq!(
        code(&unknown),
        2,
        "{}{}",
        stdout(&unknown),
        stderr(&unknown)
    );
    assert_eq!(report(unknown)["findings"][0]["id"], "resolver.selection");
    assert_eq!(std::fs::read(&preview).unwrap(), saved);
}

pub fn connected(server: &str, dialect: &str) {
    let own = OwnDatabase::new(server, "resolver606");
    let connection = own.connection();
    if dialect == "postgres" {
        on_server(connection, "CREATE SCHEMA app");
    }
    let d = Demo::new("resolver606connected");
    d.table(ONE_COLUMN);
    configure(&d, dialect);
    successful(run(&d, &["plan"], None));
    d.commit();
    successful(run(&d, &["bootstrap", "--db", connection], None));
    d.table(&ONE_COLUMN.replace("  id:", "  label: {type: varchar(50)}\n  id:"));
    successful(run(&d, &["plan"], None));
    d.commit();
    let artifact = d.dir.join("plan.json");
    for (target, cli, expected_name, expected_source, kind, pull) in [
        (
            vec!["--db", connection],
            None,
            "local",
            "project",
            "docker",
            Some("never"),
        ),
        (
            vec!["--env", "prod"],
            None,
            "scratch",
            "environment",
            "server",
            None,
        ),
        (
            vec!["--env", "stage"],
            None,
            "local",
            "project",
            "docker",
            Some("never"),
        ),
        (
            vec!["--env", "prod"],
            Some("internal"),
            "internal",
            "cli",
            "docker",
            Some("if_missing"),
        ),
    ] {
        let mut args = vec!["plan"];
        args.extend(target);
        if let Some(cli) = cli {
            args.extend(["--resolve-with", cli]);
        }
        args.extend(["--out", artifact.to_str().unwrap(), "--format", "json"]);
        let value = report(successful(run(&d, &args, Some(connection))));
        assert_eq!(value["result"], "ok");
        assert!(value["data"]["changes"].as_u64().unwrap() > 0, "{value}");
        let selected = &value["data"]["resolver_selection"];
        assert_eq!(selected["name"], expected_name, "{value}");
        assert_eq!(selected["source"], expected_source, "{value}");
        assert_eq!(selected["status"], "not_acquired", "{value}");
        assert_eq!(selected["profile"]["kind"], kind, "{value}");
        assert_eq!(selected["profile"]["pull"].as_str(), pull, "{value}");
        let saved: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&artifact).unwrap()).unwrap();
        assert!(saved.get("resolver_selection").is_none(), "{saved}");
        assert!(!saved.to_string().contains("PBPS_RESOLVER_UNSET_606"));
    }
    let human = stdout(&successful(run(
        &d,
        &["plan", "--env", "prod"],
        Some(connection),
    )));
    assert!(
        human.contains("Resolver profile: scratch") && human.contains("not acquired"),
        "{human}"
    );
    assert!(human.contains("not implemented"), "{human}");
    successful(run(&d, &["doctor", "--env", "prod"], Some(connection)));
    successful(run(
        &d,
        &["doctor", "--env", "prod", "--format", "json"],
        Some(connection),
    ));
    // Supplying a secret value still must not expose it in a selection report.
    let output = Command::new(BIN)
        .arg("--project")
        .arg(&d.dir)
        .args(["plan", "--env", "prod", "--format", "json"])
        .env("PBPS_TARGET_606", connection)
        .env("PBPS_RESOLVER_UNSET_606", "resolver-secret-606")
        .output()
        .unwrap();
    let text = stdout(&successful(output));
    assert!(!text.contains("resolver-secret-606"), "{text}");
    // The ordinary saved artifact still applies without using the configured
    // resolver. Its configuration is deliberately absent from the artifact.
    let saved: pbps_model::SavedPlan =
        serde_json::from_str(&std::fs::read_to_string(&artifact).unwrap()).unwrap();
    successful(run(
        &d,
        &[
            "explain",
            "--plan",
            artifact.to_str().unwrap(),
            "--format",
            "json",
        ],
        None,
    ));
    successful(run(
        &d,
        &[
            "apply",
            "--db",
            connection,
            "--plan",
            artifact.to_str().unwrap(),
            "--checksum",
            &saved.checksum(),
        ],
        None,
    ));
}
