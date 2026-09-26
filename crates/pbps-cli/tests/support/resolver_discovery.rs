use super::*;

/// Shared product assertions for the two real-engine CLI entry points.
pub fn check(server: &str, dialect: &str) {
    let own = OwnDatabase::new(server, "resolver597");
    let d = Demo::new("resolver597");
    d.table(ONE_COLUMN);
    d.commit();
    if dialect == "postgres" {
        on_server(own.connection(), "CREATE SCHEMA app");
    }
    let connection = own.connection();
    let run = |args: &[&str]| {
        let mut cmd = Command::new(BIN);
        cmd.arg("--project").arg(&d.dir).args(args);
        // No Docker command (including image/registry inspection) may run.
        // A PATH trap records even ignored failures; missing Docker alone
        // would not prove that the command never tried to invoke it.
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
            let path = std::env::var_os("PATH").unwrap();
            let mut paths = vec![bin];
            paths.extend(std::env::split_paths(&path));
            cmd.env("PATH", std::env::join_paths(paths).unwrap());
            cmd.env("PBPS_RESOLVER_DOCKER_MARKER", d.dir.join("docker-invoked"));
        }
        cmd.output().unwrap()
    };
    let output = run(&["doctor", "--db", connection, "--format", "json"]);
    assert_eq!(code(&output), 0, "{}{}", stdout(&output), stderr(&output));
    let report: serde_json::Value = serde_json::from_str(&stdout(&output)).unwrap();
    let environment = &report["data"]["environments"][0];
    let discovery = &environment["resolver"];
    assert_eq!(environment["state"], "uninitialized");
    assert_eq!(discovery["compatibility"], "unverified", "{report}");
    assert_eq!(discovery["candidate"]["status"], "suggested", "{report}");
    let field = if dialect == "postgres" {
        "database_encoding"
    } else {
        "database_compatibility_level"
    };
    assert_eq!(
        discovery["observations"][field]["status"], "observed",
        "{report}"
    );
    assert_eq!(
        discovery["qualification"]["deployment-context"]["status"],
        "unknown"
    );
    assert_eq!(
        discovery["qualification"]["binding-adapter"]["status"],
        "unknown"
    );
    let schema: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../schemas/envelope.schema.json"),
        )
        .unwrap(),
    )
    .unwrap();
    assert!(
        jsonschema::validator_for(&schema)
            .unwrap()
            .is_valid(&report),
        "{report}"
    );
    crate::envelope_archives::assert_accepted_by_archives(&report, "resolver discovery");
    let human = run(&["doctor", "--db", connection]);
    assert_eq!(code(&human), 0, "{}{}", stdout(&human), stderr(&human));
    let text = stdout(&human);
    assert!(
        text.contains("resolver compatibility: unverified"),
        "{text}"
    );
    assert!(text.contains("not acquired or verified"), "{text}");
    assert!(
        text.contains("Binding resolution is not implemented"),
        "{text}"
    );
    assert!(text.contains("introspection connection"), "{text}");
    assert!(!d.dir.join("docker-invoked").exists());
    assert!(
        !d.dir.join("schema.ids.json").exists(),
        "doctor wrote identity"
    );
    assert!(!text.contains("Password=") && !text.contains("password="));

    // The new metadata read has its own unanswerable path, independently of
    // successful version/capability reads. Restrict only this test database.
    if dialect == "postgres" {
        let reader = format!("pbps_discovery597_{}", std::process::id());
        on_server(
            connection,
            &format!(
                "CREATE ROLE {reader} LOGIN PASSWORD 'discovery-test'; REVOKE SELECT ON pg_catalog.pg_extension FROM PUBLIC"
            ),
        );
        let restricted = format!("{connection} user={reader} password=discovery-test");
        let denied = run(&["doctor", "--db", &restricted, "--format", "json"]);
        on_server(connection, &format!("DROP ROLE {reader}"));
        assert_eq!(code(&denied), 1, "{}{}", stdout(&denied), stderr(&denied));
        let denied: serde_json::Value = serde_json::from_str(&stdout(&denied)).unwrap();
        assert!(denied["data"]["environments"][0].get("resolver").is_none());
        assert!(
            denied["findings"]
                .as_array()
                .unwrap()
                .iter()
                .any(|f| f["id"] == "resolver.discovery-unknown"),
            "{denied}"
        );
        assert!(!denied.to_string().contains("discovery-test"));
    }
}
