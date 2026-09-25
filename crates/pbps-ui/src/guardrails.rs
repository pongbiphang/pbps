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

/// The commands a route may run and the options it may pass, by name. A
/// browser value reaches a child only as the value of one of these options,
/// or as an operand after `--`, never as an argument of its own, where the
/// CLI could read it as a command, a flag or a statement. `--db` and anything
/// SQL-shaped are absent by construction.
const COMMANDS: [&str; 12] = [
    "status",
    "verify",
    "explain",
    "state",
    "docs",
    "plan",
    "rename",
    "rename-table",
    "rename-role",
    "drop",
    "drop-table",
    "drop-role",
];
const OPTIONS: [&str; 5] = ["--env", "--plan", "--reason", "--since", "--format"];
const FLAGS: [&str; 4] = ["list", "--no-dev", "--format=json", "--format=html"];

/// Whether `arguments` are one known command followed only by known flags,
/// known options, and operands after a single `--`, with `value` appearing
/// only as a whole option value or operand.
fn within_the_vocabulary(arguments: &[String], value: &str) -> Result<(), String> {
    let command = arguments.first().ok_or("no command")?;
    if !COMMANDS.contains(&command.as_str()) {
        return Err(format!("command {command:?}"));
    }
    let separator = arguments.iter().position(|a| a == "--");
    for (index, argument) in arguments.iter().enumerate().skip(1) {
        if separator.is_some_and(|at| index > at) {
            // An operand: the CLI reads nothing after `--` as an option.
            continue;
        }
        if argument == "--" || FLAGS.contains(&argument.as_str()) {
            continue;
        }
        match argument.split_once('=') {
            Some((flag, _)) if OPTIONS.contains(&flag) => {}
            _ => return Err(format!("argument {argument:?}")),
        }
        if argument.contains(value) && argument.split_once('=').map(|(_, v)| v) != Some(value) {
            return Err(format!("{value:?} is not the whole value of {argument:?}"));
        }
    }
    if arguments.iter().filter(|a| *a == "--").count() > 1 {
        return Err("more than one `--`".into());
    }
    Ok(())
}

#[test]
fn an_argument_outside_the_vocabulary_is_refused() {
    let args = |list: &[&str]| list.iter().map(|a| (*a).to_owned()).collect::<Vec<_>>();
    assert_eq!(
        within_the_vocabulary(&args(&["verify", "--env=x", "--format=json"]), "x"),
        Ok(())
    );
    assert_eq!(
        within_the_vocabulary(&args(&["rename", "--", "x", "--db=y"]), "x"),
        Ok(())
    );
    for bad in [
        &["verify", "--db=x"][..],
        &["verify", "--sql=x"],
        &["verify", "--env=x", "x"],
        &["verify", "--env=prefix x"],
        &["sql", "--env=x"],
        &["apply", "--env=x"],
        &["rename", "--", "x", "--", "y"],
        &[],
    ] {
        assert!(within_the_vocabulary(&args(bad), "x").is_err(), "{bad:?}");
    }
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
            assert_eq!(
                within_the_vocabulary(&arguments, SQL),
                Ok(()),
                "{path}: {arguments:?}"
            );
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

/// Compose's named fields reach the CLI, too: identifiers as operands after
/// `--`, where the CLI can read nothing as an option, and a drop reason as
/// its own `--reason=` value. Each intent comes from JSON, as the browser
/// sends it, with SQL in every string field.
#[cfg(target_os = "linux")]
#[test]
fn compose_intent_fields_reach_the_cli_only_as_operands_or_one_option_value() {
    use compose::Intent;
    // Exhaustive on purpose: a new intent fails to compile here until it is
    // added to the bodies below.
    fn kind(intent: &Intent) -> &'static str {
        match intent {
            Intent::Rename { .. } => "rename",
            Intent::RenameTable { .. } => "rename-table",
            Intent::Drop { .. } => "drop",
            Intent::DropTable { .. } => "drop-table",
            Intent::RenameRole { .. } => "rename-role",
            Intent::DropRole { .. } => "drop-role",
            Intent::Declarations => "declarations",
        }
    }
    let bodies = [
        serde_json::json!({"kind": "rename", "from": SQL, "to": SQL}),
        serde_json::json!({"kind": "rename-table", "from": SQL, "to": SQL}),
        serde_json::json!({"kind": "drop", "column": SQL, "reason": SQL}),
        serde_json::json!({"kind": "drop-table", "table": SQL, "reason": SQL}),
        serde_json::json!({"kind": "rename-role", "from": SQL, "to": SQL}),
        serde_json::json!({"kind": "drop-role", "role": SQL, "reason": SQL}),
        serde_json::json!({"kind": "declarations"}),
    ];
    let mut kinds = Vec::new();
    for body in bodies {
        let intent: Intent = serde_json::from_value(body).unwrap();
        kinds.push(kind(&intent));
        let arguments = intent.arguments(SQL);
        assert_eq!(
            within_the_vocabulary(&arguments, SQL),
            Ok(()),
            "{arguments:?}"
        );
    }
    let mut unique = kinds.clone();
    unique.dedup();
    assert_eq!(unique.len(), 7, "every intent is exercised: {kinds:?}");
    // And a field the intent does not name is refused.
    assert!(
        serde_json::from_value::<Intent>(
            serde_json::json!({"kind": "rename", "from": "a", "to": "b", "sql": SQL})
        )
        .is_err()
    );
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

/// Whether a source line could read an environment variable's value.
/// Conservative on purpose: any path through `env` counts, however it is
/// imported or aliased (`use std::env::var;`, `use std::{env, ..}`), and only
/// `env::temp_dir()`, which yields a path, passes.
fn reaches_environment(line: &str) -> bool {
    let words: Vec<&str> = line
        .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .collect();
    let import = line.trim_start().starts_with("use ");
    // A name that reads the environment by itself, or any import of `env`.
    // The compile-time macros embed a variable's value in the binary; only
    // Cargo's own manifest directory, a build path, is allowed.
    // Each invocation on its own: an allowed one beside another on the same
    // line must not excuse it. `option_env!(` ends in `env!(` and is split
    // the same way.
    let embedded = line
        .split("env!(")
        .skip(1)
        .any(|rest| !rest.starts_with("\"CARGO_MANIFEST_DIR\")"));
    let direct = embedded
        || (import && words.contains(&"env"))
        || words
            .iter()
            .any(|w| matches!(*w, "getenv" | "environ" | "vars_os" | "var_os"));
    // A path through `env`, which passes only when every one is `temp_dir()`.
    let path = (line.contains("env::") || line.contains("::env"))
        && !line
            .split("env::")
            .skip(1)
            .all(|rest| rest.starts_with("temp_dir("));
    direct || path
}

#[test]
fn an_aliased_or_imported_environment_read_is_still_seen() {
    for line in [
        "use std::env::var;",
        "use std::env;",
        "use std::{env, fs};",
        "use std::{fs, env::var_os};",
        "use std::env::temp_dir;",
        "let url = std::env::var(\"PBPS_DB\");",
        "let url = env::var_os(name);",
        "for (k, v) in std::env::vars() {",
        "let p = libc::getenv(name);",
        "const URL: &str = env!(\"PBPS_DB\");",
        "let url = option_env!(\"PBPS_DB\");",
        "let url = core::env!(\"PBPS_DB\");",
        "let _ = (env!(\"CARGO_MANIFEST_DIR\"), env!(\"PBPS_DB\"));",
        "let _ = (env!(\"CARGO_MANIFEST_DIR\"), option_env!(\"PBPS_DB\"));",
        "let p = std::env::temp_dir().join(std::env::var(\"X\").unwrap());",
    ] {
        assert!(reaches_environment(line), "{line}");
    }
    for line in [
        "let path = std::env::temp_dir().join(\"x\");",
        "command.env_remove(name);",
        ".env_clear()",
        "let root = std::path::Path::new(env!(\"CARGO_MANIFEST_DIR\"));",
        "let environment = run.environment.clone();",
        "(\"/api/drift\", Some(\"env\"), View::Drift),",
    ] {
        assert!(!reaches_environment(line), "{line}");
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
            if reaches_environment(line) {
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
