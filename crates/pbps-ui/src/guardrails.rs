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
        // A route without a parameter still runs only its fixed vocabulary.
        if parameter.is_none() {
            let view = route(path).unwrap();
            let arguments = view.arguments();
            assert_eq!(
                within_the_vocabulary(&arguments, SQL),
                Ok(()),
                "{path}: {arguments:?}"
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

/// The one environment read the UI makes: compose removing inherited
/// `GIT_*` overrides from Git's environment by name, discarding each value.
const NAMES_ONLY_SCRUB: &str = "letnames=std::env::vars_os().map(|(name,_)|name);";

/// Rust source without its comments and whitespace, so a path or macro split
/// by either (`std::env/**/::var`, `env ! (`) reads as one run of tokens.
/// String, raw-string and char literals are kept, so a `//` inside
/// `"http://"` is not taken for a comment.
fn normalized(source: &str) -> String {
    let chars: Vec<char> = source.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    let at = |i: usize| chars.get(i).copied();
    while let Some(c) = at(i) {
        match c {
            '/' if at(i + 1) == Some('/') => {
                while at(i).is_some_and(|c| c != '\n') {
                    i += 1;
                }
            }
            '/' if at(i + 1) == Some('*') => {
                let mut depth = 0;
                while let Some(c) = at(i) {
                    if c == '/' && at(i + 1) == Some('*') {
                        depth += 1;
                        i += 2;
                    } else if c == '*' && at(i + 1) == Some('/') {
                        depth -= 1;
                        i += 2;
                        if depth == 0 {
                            break;
                        }
                    } else {
                        i += 1;
                    }
                }
            }
            'r' if {
                let mut j = i + 1;
                while at(j) == Some('#') {
                    j += 1;
                }
                // `r"`, and the raw byte and C strings `br"` and `cr"`: the
                // `r` starts a literal unless it ends an identifier.
                let word = |k: usize| at(k).is_some_and(|p| p.is_alphanumeric() || p == '_');
                let prefixed =
                    matches!(at(i.wrapping_sub(1)), Some('b' | 'c')) && !word(i.wrapping_sub(2));
                at(j) == Some('"') && (!word(i.wrapping_sub(1)) || prefixed)
            } =>
            {
                let mut hashes = 0;
                i += 1;
                while at(i) == Some('#') {
                    hashes += 1;
                    i += 1;
                }
                i += 1;
                out.push('"');
                while let Some(c) = at(i) {
                    if c == '"' && (1..=hashes).all(|k| at(i + k) == Some('#')) {
                        i += 1 + hashes;
                        break;
                    }
                    if !c.is_whitespace() {
                        out.push(c);
                    }
                    i += 1;
                }
                out.push('"');
            }
            '"' => {
                out.push('"');
                i += 1;
                while let Some(c) = at(i) {
                    i += 1;
                    if c == '\\' {
                        if let Some(escaped) = at(i) {
                            out.push(c);
                            out.push(escaped);
                            i += 1;
                        }
                        continue;
                    }
                    if !c.is_whitespace() {
                        out.push(c);
                    }
                    if c == '"' {
                        break;
                    }
                }
            }
            // A char literal, which may hold a quote; a lifetime is copied.
            '\'' if at(i + 1) == Some('\\') || at(i + 2) == Some('\'') => {
                let end = (i + 2..chars.len())
                    .find(|&j| chars[j] == '\'' && chars[j - 1] != '\\')
                    .unwrap_or(chars.len() - 1);
                out.extend(&chars[i..=end]);
                i = end + 1;
            }
            c if c.is_whitespace() => i += 1,
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    out
}

#[test]
fn normalizing_drops_comments_and_whitespace_but_keeps_literals() {
    assert_eq!(normalized("std::env/**/::var(x)"), "std::env::var(x)");
    assert_eq!(
        normalized("std::env // note\n  ::var(x)"),
        "std::env::var(x)"
    );
    assert_eq!(normalized("a /* outer /* inner */ still */ b"), "ab");
    assert_eq!(normalized("f(\"http://x\"); g()"), "f(\"http://x\");g()");
    assert_eq!(normalized("r#\"a // b\"# c"), "\"a//b\"c");
    assert_eq!(
        normalized("let x = br#\"\"//\"#; let y = std::env::var(z);"),
        "letx=b\"\"//\";lety=std::env::var(z);"
    );
    assert_eq!(normalized("c r\"//\" z"), "c\"//\"z");
    assert_eq!(normalized("cr#\"//\"# z"), "c\"//\"z");
    assert_eq!(normalized("let q = '\"'; env!(x)"), "letq='\"';env!(x)");
    assert_eq!(normalized("let e = '\\''; /* c */ x"), "lete='\\'';x");
    assert_eq!(normalized("fn f<'a>(x: &'a str) {}"), "fnf<'a>(x:&'astr){}");
}

/// Every lint attribute in `source` that could silence the clippy rules in
/// `clippy.toml`: by name, through the `style` or `all` group they belong
/// to, or with `warnings`. Read from normalized source, so `cfg_attr` forms
/// are seen as well.
fn lint_suppressions(source: &str) -> Vec<String> {
    let text = normalized(source);
    let mut found = Vec::new();
    for opener in ["allow(", "expect("] {
        for (at, _) in text.match_indices(opener) {
            let body = &text[at..];
            let body = &body[..body.find(')').map_or(body.len(), |end| end + 1)];
            if [
                "disallowed_methods",
                "disallowed_macros",
                "clippy::style",
                "clippy::all",
                "warnings",
            ]
            .iter()
            .any(|lint| body.contains(lint))
            {
                found.push(body.to_owned());
            }
        }
    }
    found
}

#[test]
fn a_suppression_of_the_environment_lints_is_seen_in_any_form() {
    for source in [
        "#[allow(clippy::disallowed_methods)] fn f() {}",
        "#[expect(clippy::disallowed_macros, reason = \"x\")] fn f() {}",
        "#![allow(clippy::style)]",
        "#[allow(clippy::all)] fn f() {}",
        "#![allow(warnings)]",
        "#[cfg_attr(unix, allow(clippy::disallowed_methods))] fn f() {}",
        "#[ allow ( clippy :: disallowed_methods ) ] fn f() {}",
    ] {
        assert!(!lint_suppressions(source).is_empty(), "{source}");
    }
    for source in [
        "#[allow(dead_code)] fn f() {}",
        "#[expect(clippy::too_many_arguments, reason = \"x\")] fn f() {}",
        "let v = x.expect(\"a value\");",
    ] {
        assert!(lint_suppressions(source).is_empty(), "{source}");
    }
}

/// Every place in `source` that could read an environment variable's value.
/// The source is compared with all whitespace removed, since Rust accepts
/// `env ! ("X")` and `std :: env :: var` split across lines. Conservative on
/// purpose: any path through `env` counts however it is imported or aliased,
/// and only `env::temp_dir()` (a path) and `env!("CARGO_MANIFEST_DIR")` (a
/// build path) pass.
fn environment_reads(source: &str) -> Vec<String> {
    let text = normalized(source);
    let mut found = Vec::new();
    let mut each = |pattern: &str, allowed: &dyn Fn(&str) -> bool| {
        for (at, _) in text.match_indices(pattern) {
            let rest = &text[at + pattern.len()..];
            if !allowed(rest) {
                found.push(text[at..].chars().take(40).collect::<String>());
            }
        }
    };
    // `env!(` also ends `option_env!(`.
    each("env!(", &|rest| rest.starts_with("\"CARGO_MANIFEST_DIR\")"));
    each("env::", &|rest| rest.starts_with("temp_dir("));
    // `std::env` as a module in a `use`, not followed by a path.
    each("::env", &|rest| rest.starts_with("::"));
    // `env` in a `use` group, alone or aliased: `std::{env, ..}`,
    // `std::{.., env}`, `std::{env as e}`. A bare `{env}` is a format
    // placeholder, so a group has to follow `::`. With whitespace removed an
    // alias reads `envas…`, so `as` after `env` counts as the end of the name.
    let ends_a_name =
        |rest: &str| rest.starts_with(',') || rest.starts_with('}') || rest.starts_with("as");
    each("::{env", &|rest| !ends_a_name(rest));
    each(",env", &|rest| !ends_a_name(rest));
    for word in ["getenv", "environ(", "var_os", "vars_os"] {
        each(word, &|_| false);
    }
    found
}

#[test]
fn an_aliased_imported_or_spaced_environment_read_is_still_seen() {
    for source in [
        "use std::env::var;",
        "use std::env;",
        "use std::{env, fs};",
        "use std::{fs, env};",
        "use std::{fs, env::var_os};",
        "use std::{env as e};\nlet value = e::var(\"PBPS_DB\");",
        "use std::{fs, env as e};",
        "use std::{env as e, fs};",
        "use std::env::temp_dir;\nlet x = temp_dir();",
        "let url = std::env::var(\"PBPS_DB\");",
        "let url = std :: env :: var(\"PBPS_DB\");",
        "let url = std::env\n    ::var(\"PBPS_DB\");",
        "let url = env::var_os(name);",
        "for (k, v) in std::env::vars() {",
        "let p = libc::getenv(name);",
        "let p = std::env::temp_dir().join(std::env::var(\"X\").unwrap());",
        "const URL: &str = env!(\"PBPS_DB\");",
        "const URL: &str = env ! (\"PBPS_DB\");",
        "const URL: &str = env\n!\n(\"PBPS_DB\");",
        "let url = option_env!(\"PBPS_DB\");",
        "let url = core::env!(\"PBPS_DB\");",
        "let url = std::env/**/::var(\"PBPS_DB\");",
        "let url = option_env/**/!(\"PBPS_DB\");",
        "let url = std::env // why\n    ::var(\"PBPS_DB\");",
        "let _ = (env!(\"CARGO_MANIFEST_DIR\"), env!(\"PBPS_DB\"));",
    ] {
        assert!(!environment_reads(source).is_empty(), "{source}");
    }
    for source in [
        "let path = std::env::temp_dir().join(\"x\");",
        "let path = std :: env :: temp_dir();",
        "command.env_remove(name);",
        ".env_clear()",
        "let environment = run.environment.clone();",
        "(\"/api/drift\", Some(\"env\"), View::Drift),",
        "format!(\"--env={env}\")",
        "use std::{environment_free, fs};",
        "f(a, environment)",
        "let root = std::path::Path::new(env!(\"CARGO_MANIFEST_DIR\"));",
        "let names = std::env::vars_os().map(|(name, _)| name);",
    ] {
        let source = normalized(source).replace(NAMES_ONLY_SCRUB, "");
        assert!(environment_reads(&source).is_empty(), "{source}");
    }
}

/// The source scan cannot see a path a macro assembles during expansion;
/// clippy's `disallowed_methods` and `disallowed_macros`, which resolve paths
/// after expansion, can, and CI runs clippy with `-D warnings`. This pins the
/// configuration those lints read, so deleting it fails a test.
#[test]
fn clippy_forbids_every_environment_read_in_this_crate() {
    let config = include_str!("../clippy.toml");
    for path in [
        "std::env::var\"",
        "std::env::var_os\"",
        "std::env::vars\"",
        "std::env::vars_os\"",
        "std::env\"",
        "std::option_env\"",
    ] {
        assert!(config.contains(&format!("path = \"{path}")), "{path}");
    }
    assert!(config.contains("disallowed-methods") && config.contains("disallowed-macros"));
}

/// A Cargo lint level overrides CI's `-D warnings`, so the manifests are a
/// second place these lints could be silenced. This crate inherits the
/// workspace's lints and sets none of its own, and the workspace names none
/// of these lints, their `style` or `all` group, or `warnings` at any level.
#[test]
fn no_manifest_lowers_the_environment_lints() {
    let crate_manifest = include_str!("../Cargo.toml");
    let lints = crate_manifest
        .split("[lints")
        .skip(1)
        .collect::<Vec<_>>()
        .join("[lints");
    assert_eq!(lints.trim(), "]\nworkspace = true", "{crate_manifest}");
    let workspace = include_str!("../../../Cargo.toml");
    for line in workspace
        .lines()
        .filter(|line| !line.trim_start().starts_with('#'))
    {
        let key = line.split('=').next().unwrap_or("").trim();
        assert!(
            !matches!(
                key,
                "disallowed_methods"
                    | "disallowed-methods"
                    | "disallowed_macros"
                    | "disallowed-macros"
                    | "style"
                    | "all"
                    | "warnings"
            ),
            "the workspace manifest sets {line:?}"
        );
    }
}

/// ADR-0015 decision 4: the UI process never holds a connection string. The
/// child reads `url_env` itself, and the UI must not read any environment
/// value. The one read is compose's names-only scrub, removed by its exact
/// text before the scan and required to be where it is.
#[test]
#[expect(
    clippy::disallowed_macros,
    reason = "the test locates this crate's sources"
)]
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
    let mut scrubs = 0;
    let mut suppressions = Vec::new();
    for file in files {
        if file.ends_with("guardrails.rs") {
            continue;
        }
        // Joined with `/` on every platform: `display()` gives `compose\git.rs`
        // on Windows, which would never match the scrub's file.
        let name = file
            .strip_prefix(&root)
            .unwrap()
            .components()
            .map(|part| part.as_os_str().to_string_lossy())
            .collect::<Vec<_>>()
            .join("/");
        let source = std::fs::read_to_string(&file).unwrap();
        for suppression in lint_suppressions(&source) {
            suppressions.push((name.clone(), suppression));
        }
        let text = normalized(&source);
        let without = if name == "compose/git.rs" {
            scrubs += text.matches(NAMES_ONLY_SCRUB).count();
            text.replace(NAMES_ONLY_SCRUB, "")
        } else {
            text
        };
        for read in environment_reads(&without) {
            reads.push((name.clone(), read));
        }
    }
    assert_eq!(scrubs, 1, "the names-only scrub is where it is expected");
    // The lints in `clippy.toml` are silenced in exactly one place: the scrub
    // statement. Any other `allow` or `expect` would let a read the source
    // scan cannot see, such as one a macro assembles, through CI's clippy.
    assert_eq!(
        suppressions,
        [(
            "compose/git.rs".to_owned(),
            "expect(clippy::disallowed_methods,reason=\"readsonlythenamesofinheritedvariablesanddiscardseveryvalue\")"
                .to_owned()
        )]
    );
    assert_eq!(reads, Vec::<(String, String)>::new());
}
