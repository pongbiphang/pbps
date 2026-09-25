//! The three refusals of ADR-0006, tested across every route the viewer
//! answers (#64 step 6, #1050). Each test enumerates the routes from the
//! tables the router itself reads (`SHELL`, `READS`, `COMPOSE_ACTIONS`,
//! `trigger::ACTIONS`), so a route added later falls under it without anyone
//! remembering to add it here.

use super::*;

const SQL: &str = "DROP TABLE dbo.t; --";

fn scratch(name: &str) -> PathBuf {
    let directory =
        std::env::temp_dir().join(format!("pbps-ui-guardrails-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&directory);
    std::fs::create_dir_all(&directory).unwrap();
    directory
}

/// A value from the browser may reach a child only as the value of one
/// `--flag=` option of the route's fixed vocabulary, never as an argument of
/// its own, where the CLI could read it as a command, a flag or a statement.
fn carried_only_as_an_option_value(arguments: &[String], value: &str) -> bool {
    arguments.iter().all(|argument| {
        !argument.contains(value)
            || argument
                .split_once('=')
                .is_some_and(|(flag, rest)| flag.starts_with("--") && rest == value)
    })
}

#[test]
fn no_read_route_accepts_sql_as_a_parameter_or_carries_it_as_an_argument() {
    let escaped = SQL.bytes().map(|b| format!("%{b:02X}")).collect::<String>();
    for (path, parameter, _) in READS {
        // An unknown parameter is refused, whatever it is called.
        for key in ["sql", "query", "statement", "db"] {
            assert!(
                route(&format!("{path}?{key}={escaped}")).is_err(),
                "{path} {key}"
            );
        }
        // The parameter a route takes carries SQL only as its own value.
        if let Some(parameter) = parameter {
            let view = route(&format!("{path}?{parameter}={escaped}")).unwrap();
            let arguments = view.arguments();
            assert!(
                carried_only_as_an_option_value(&arguments, SQL),
                "{path}: {arguments:?}"
            );
            assert!(arguments[0].chars().all(|c| c.is_ascii_lowercase()));
        }
    }
}

#[test]
fn no_write_action_accepts_a_field_it_does_not_name() {
    let directory = scratch("fields");
    let executable = directory.join("never-started");
    let body = serde_json::to_vec(&serde_json::json!({ "sql": SQL })).unwrap();
    let mut trigger = trigger::Trigger::new(executable.clone(), directory.clone());
    for action in trigger::ACTIONS {
        let refused = trigger.answer(action, &body).unwrap_err();
        assert_eq!(refused.0, 400, "trigger {action}: {}", refused.1);
    }
    // A well-formed request plus one unnamed field is still refused.
    let mut apply = serde_json::json!({"environment": "prod", "plan": "p.json",
        "checksum": "c", "allow": [], "staged": false, "resume": false});
    apply["sql"] = SQL.into();
    let refused = trigger
        .answer("apply", &serde_json::to_vec(&apply).unwrap())
        .unwrap_err();
    assert_eq!(refused.0, 400, "{}", refused.1);
    // Nothing was started by any of them.
    let runs: Vec<serde_json::Value> =
        serde_json::from_slice(&trigger.answer("runs", b"{}").unwrap()).unwrap();
    assert!(runs.is_empty());

    #[cfg(target_os = "linux")]
    {
        let mut compose = compose_http::Compose::new(executable, directory.clone());
        for action in COMPOSE_ACTIONS {
            let refused = compose.answer(action, &body).unwrap_err();
            assert_eq!(refused.0, 400, "compose {action}: {}", refused.1);
        }
    }
    let _ = std::fs::remove_dir_all(&directory);
}

#[test]
fn the_route_tables_are_the_whole_router() {
    let peer = Some("127.0.0.1:1234".parse().unwrap());
    let public = |url: &str| {
        authorized(
            peer,
            "GET",
            url,
            &[("Host", "127.0.0.1:8080")],
            "127.0.0.1:8080",
            "secret",
        )
    };
    for (path, ..) in SHELL {
        assert!(public(path), "{path}");
        for near in [
            format!("{path}x"),
            format!("{path}?x=1"),
            path.to_uppercase(),
        ] {
            if near != path {
                assert!(!public(&near), "{near}");
            }
        }
    }
    for (path, ..) in READS {
        for near in [
            format!("{path}x"),
            format!("{path}/"),
            path.to_uppercase(),
            path.replace("/api/", "/api//"),
        ] {
            assert!(route(&near).is_err(), "{near}");
        }
        assert!(!public(path), "a read is never public: {path}");
    }
    // Every write action is a POST-only route outside the read table.
    for action in COMPOSE_ACTIONS {
        assert!(route(&format!("/api/compose/{action}")).is_err());
    }
    for action in trigger::ACTIONS {
        assert!(route(&format!("/api/trigger/{action}")).is_err());
    }
}

/// ADR-0015 decision 4: the UI process never holds a connection string. The
/// child reads `url_env` itself, and the UI must not read any environment
/// value. The one read is compose removing inherited `GIT_*` overrides from
/// Git's environment, which takes the names and discards the values.
#[test]
fn the_ui_reads_no_environment_value() {
    fn sources(directory: &std::path::Path, found: &mut Vec<PathBuf>) {
        for entry in std::fs::read_dir(directory).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                sources(&path, found);
            } else if path.extension().is_some_and(|e| e == "rs") {
                found.push(path);
            }
        }
    }
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    sources(&root, &mut files);
    assert!(files.len() > 10, "the source walk found {files:?}");
    let mut reads = Vec::new();
    for file in files {
        if file.ends_with("guardrails.rs") {
            continue;
        }
        let text = std::fs::read_to_string(&file).unwrap();
        for line in text.lines() {
            if [
                "env::var(",
                "env::var_os(",
                "env::vars(",
                "env::vars_os(",
                "getenv",
            ]
            .iter()
            .any(|call| line.contains(call))
            {
                let name = file.strip_prefix(&root).unwrap().display().to_string();
                reads.push((name, line.trim().to_owned()));
            }
        }
    }
    assert_eq!(
        reads,
        [(
            "compose/git.rs".to_owned(),
            "for (name, _) in std::env::vars_os() {".to_owned()
        )]
    );
}
