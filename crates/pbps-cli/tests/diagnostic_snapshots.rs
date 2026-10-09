//! Complete output from the shipped CLI, with semantic assertions beside the goldens.
//! Update with INSTA_UPDATE=new, then review each .snap.new individually.

use std::{
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::atomic::{AtomicUsize, Ordering},
};

const BIN: &str = env!("CARGO_BIN_EXE_pbps");
const GOOD: &str = "table: dbo.t\ncolumns:\n  id:\n    type: int\n";
const UNICODE: &str = "table: dbo.café\ndescription: Café régulier — clients français\ncolumns:\n  identifiant:\n    type: bigint\n  solde:\n    type: \"decimal(18, 2\"\n    nullable: false\n";
const INLINE: &str = "table: dbo.t\ncolumns: {café: {type: \"decimal(18, 2\"}}\n";
const FK: &str = "table: dbo.t\ncolumns:\n  id:\n    type: int\nforeign_keys:\n  fk_a:\n    columns: [id]\n    references: dbo.region\n";

struct Project(PathBuf);

impl Project {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        // create_dir, never remove-before-create: stale or concurrent fixtures
        // cannot grant ownership of a directory created by another process.
        let root = loop {
            let path = std::env::temp_dir().join(format!(
                "pbps-diagnostic-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            match std::fs::create_dir(&path) {
                Ok(()) => break path,
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => panic!("create fixture: {e}"),
            }
        };
        let project = Self(root);
        std::fs::create_dir(project.0.join("schema")).unwrap();
        project.write("pbps.yml", "dialect: mssql\n");
        project.write("sentinel", "untouched\n");
        let output = Command::new("git")
            .args(["init", "-q"])
            .current_dir(&project.0)
            .output()
            .unwrap();
        assert!(output.status.success());
        project
    }

    fn write(&self, path: &str, text: &str) {
        std::fs::write(self.0.join(path), text).unwrap();
    }

    fn run(&self, args: &[&str], expected: i32) -> Capture {
        // terminal_size 0.4 checks precisely these three handles on Unix and
        // Windows. None is a terminal, so miette uses its 80-column fallback.
        // Relative --project keeps random temp paths out before line wrapping.
        let output = Command::new(BIN)
            .args(["--project", "."])
            .args(args)
            .current_dir(&self.0)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .env_remove("CLICOLOR_FORCE")
            .env_remove("COLORTERM")
            .env_remove("IGNORE_IS_TERMINAL")
            .env_remove("RUST_BACKTRACE")
            .env_remove("RUST_LIB_BACKTRACE")
            .env("NO_COLOR", "1")
            .env("FORCE_COLOR", "0")
            .env("CLICOLOR", "0")
            .env("FORCE_HYPERLINK", "0")
            .env("NO_GRAPHICS", "0")
            .env("TERM", "dumb")
            .output()
            .unwrap();
        let capture = Capture {
            status: output.status.code().expect("CLI terminated by signal"),
            stdout: String::from_utf8(output.stdout).expect("stdout must be UTF-8"),
            stderr: String::from_utf8(output.stderr).expect("stderr must be UTF-8"),
        };
        assert_eq!(capture.status, expected, "{capture:?}");
        assert!(!capture.stdout.contains('\x1b') && !capture.stderr.contains('\x1b'));
        assert_eq!(
            std::fs::read_to_string(self.0.join("sentinel")).unwrap(),
            "untouched\n"
        );
        capture
    }
}

impl Drop for Project {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).unwrap();
    }
}

#[derive(Debug)]
struct Capture {
    status: i32,
    stdout: String,
    stderr: String,
}
impl Capture {
    fn snapshot(&self) -> String {
        // Preserve whitespace, carets, ordering and duplicate output. The only
        // cross-platform normalization is transport line endings and the exact
        // relative fixture path token (never arbitrary backslashes).
        format!(
            "exit: {}\n--- stdout ---\n{}--- stderr ---\n{}--- end ---\n",
            self.status,
            normalize(&self.stdout),
            normalize(&self.stderr)
        )
    }

    fn json(&self, command: &str, result: &str) -> serde_json::Value {
        if result == "unanswerable" {
            assert!(
                !self.stderr.contains("pbps::load::") && !self.stderr.contains('╭'),
                "{self:?}"
            );
        } else {
            assert!(self.stderr.is_empty(), "{self:?}");
        }
        let value: serde_json::Value = serde_json::from_str(&self.stdout).unwrap();
        assert_eq!(value["schema_version"], 1);
        assert_eq!(value["command"], command);
        assert_eq!(value["result"], result);
        value
    }
}

fn normalize(text: &str) -> String {
    let mut text = text.replace("\r\n", "\n");
    for path in [
        "schema.ids.json",
        "schema/dbo.t.yml",
        "schema/dbo.café.yml",
        "schema/a.yml",
        "schema/z.yml",
    ] {
        text = text.replace(
            &format!(".\\{}", path.replace('/', "\\")),
            &format!("./{path}"),
        );
    }
    text
}

#[test]
fn normalization_preserves_diagnostic_content() {
    let text = "  .\\schema\\dbo.t.yml:1\r\n  ^ label\r\n    pbps fmt\r\n\r\n";
    assert_eq!(
        normalize(text),
        "  ./schema/dbo.t.yml:1\n  ^ label\n    pbps fmt\n\n"
    );
    for changed in [
        text.replace("  ^", "   ^"),
        text.replace("pbps fmt", "pbps plan"),
        text.repeat(2),
    ] {
        assert_ne!(normalize(&changed), normalize(text));
    }
    assert_eq!(normalize("unrelated\\path"), "unrelated\\path");
}

#[test]
fn semantic_cli_preserves_unicode_span_and_label() {
    for (name, yaml) in [("unicode", UNICODE), ("same_line_unicode", INLINE)] {
        let project = Project::new();
        project.write("schema/dbo.café.yml", yaml);
        let errors =
            pbps_load::load_file_str(Path::new("./schema/dbo.café.yml"), yaml).unwrap_err();
        assert_eq!(errors.len(), 1);
        let pbps_load::LoadError::Semantic(error) = &errors[0] else {
            panic!("expected semantic diagnostic")
        };
        let scalar = "\"decimal(18, 2\"";
        let offset = yaml.find(scalar).unwrap();
        assert!(yaml.is_char_boundary(offset));
        assert_eq!(error.span.offset(), offset);
        assert_eq!(error.span.len(), scalar.len());
        assert_eq!(
            &yaml[error.span.offset()..error.span.offset() + error.span.len()],
            scalar
        );
        let human = project.run(&["validate"], 2);
        assert!(human.stderr.contains(&error.label), "{human:?}");
        assert!(human.stderr.contains("decimal(18, 2"));
        assert!(human.stderr.contains("dbo.café.yml:"), "{human:?}");
        insta::assert_snapshot!(name, human.snapshot());
        let json = project
            .run(&["validate", "--format", "json"], 2)
            .json("validate", "findings");
        assert_eq!(json["findings"][0]["id"], "load.semantic");
        assert_eq!(json["findings"][0]["severity"], "error");
        assert_eq!(json["findings"][0]["message"], error.message);
        assert_eq!(json["findings"][0]["location"]["line"], error.line);
        assert!(json["findings"][0].get("remedy").is_none());
        project.write(
            "schema/dbo.café.yml",
            &yaml.replace("decimal(18, 2", "decimal(18, 2)"),
        );
        project.run(&["validate"], 0);
    }
}

#[test]
fn semantic_cli_preserves_real_foreign_key_help() {
    let project = Project::new();
    project.write("schema/dbo.t.yml", FK);
    let errors = pbps_load::load_file_str(Path::new("./schema/dbo.t.yml"), FK).unwrap_err();
    let pbps_load::LoadError::Semantic(error) = &errors[0] else {
        panic!("expected semantic diagnostic")
    };
    let help = error
        .help
        .as_ref()
        .expect("malformed reference must offer its syntax");
    assert!(help.contains("schema.table"));
    let human = project.run(&["validate"], 2);
    assert!(
        human
            .stderr
            .contains("help: the format is `schema.table(column)`"),
        "{human:?}"
    );
    assert!(human.stderr.contains("commas"));
    insta::assert_snapshot!("foreign_key_help", human.snapshot());
    project.write(
        "schema/dbo.t.yml",
        &FK.replace("dbo.region", "dbo.region(id)"),
    );
    project.write(
        "schema/dbo.region.yml",
        "table: dbo.region\ncolumns:\n  id:\n    type: int\n    nullable: false\nprimary_key: [id]\n",
    );
    project.run(&["validate"], 0);
}

#[test]
fn structural_cli_renders_one_excerpt() {
    for (name, yaml, excerpt) in [
        (
            "multiline_yaml",
            "table: dbo.t\ncolumns:\n  id:\n    type: [\n      int\n",
            "type: [",
        ),
        (
            "duplicate_key",
            "table: dbo.t\ncolumns:\n  id: {type: int}\n  id: {type: bigint}\n",
            "id: {type: bigint}",
        ),
        (
            "unknown_field",
            "table: dbo.t\ncolumns:\n  id:\n    type: int\n    nulable: false\n",
            "nulable: false",
        ),
    ] {
        let project = Project::new();
        project.write("schema/dbo.t.yml", yaml);
        let human = project.run(&["validate"], 2);
        assert_eq!(human.stderr.matches(excerpt).count(), 1, "{human:?}");
        insta::assert_snapshot!(name, human.snapshot());
        let json = project
            .run(&["validate", "--format", "json"], 2)
            .json("validate", "findings");
        assert_eq!(json["findings"][0]["id"], "load.yaml");
        assert!(json["findings"][0]["location"].get("line").is_none());
        project.write("schema/dbo.t.yml", GOOD);
        project.run(&["validate"], 0);
    }
}

#[test]
fn unreadable_declaration_is_distinct_from_invalid_yaml() {
    let project = Project::new();
    // A selected, existing file with invalid UTF-8 fails read_to_string even as
    // root; chmod is not a portable unreadability fixture.
    std::fs::write(project.0.join("schema/dbo.t.yml"), [0xff, 0xfe]).unwrap();
    let human = project.run(&["validate"], 2);
    insta::assert_snapshot!("unreadable_utf8", human.snapshot());
    let json = project
        .run(&["validate", "--format", "json"], 2)
        .json("validate", "findings");
    assert_eq!(json["findings"][0]["id"], "load.io");
    project.write("schema/dbo.t.yml", "");
    let empty = project
        .run(&["validate", "--format", "json"], 2)
        .json("validate", "findings");
    assert_eq!(empty["findings"][0]["id"], "load.yaml");
    std::fs::remove_file(project.0.join("schema/dbo.t.yml")).unwrap();
    let absent = project
        .run(&["validate", "--format", "json"], 0)
        .json("validate", "ok");
    assert!(absent["findings"].as_array().unwrap().is_empty());
    project.write("schema/dbo.t.yml", GOOD);
    project.run(&["validate"], 0);
}

#[test]
fn fmt_remedy_is_copyable_and_repairs_the_fixture() {
    let project = Project::new();
    let original = "table:  dbo.t\ncolumns:\n  id:  {type: INT}\n";
    project.write("schema/dbo.t.yml", original);
    let human = project.run(&["fmt", "--check"], 2);
    insta::assert_snapshot!("fmt_remedy", human.snapshot());
    let json = project
        .run(&["fmt", "--check", "--format", "json"], 2)
        .json("fmt", "findings");
    let remedy = json["findings"][0]["remedy"].as_str().unwrap();
    assert_eq!(remedy, "pbps fmt");
    assert!(human.stderr.lines().any(|line| line == "    pbps fmt"));
    assert_eq!(
        std::fs::read_to_string(project.0.join("schema/dbo.t.yml")).unwrap(),
        original
    );
    let args: Vec<_> = remedy.split_whitespace().collect();
    project.run(&args[1..], 0);
    project.run(&["fmt", "--check"], 0);
    project.run(&["validate"], 0);
    assert_eq!(
        std::fs::read_to_string(project.0.join("pbps.yml")).unwrap(),
        "dialect: mssql\n"
    );
    assert!(!project.0.join("schema.ids.json").exists());
}

#[test]
fn human_json_routes_keep_command_outcomes() {
    for (case, bytes, id) in [
        ("semantic", FK.as_bytes(), "load.semantic"),
        (
            "yaml",
            b"table: dbo.t\ncolumns: [\n".as_slice(),
            "load.yaml",
        ),
        ("io", [0xff, 0xfe].as_slice(), "load.io"),
    ] {
        let project = Project::new();
        std::fs::write(project.0.join("schema/dbo.t.yml"), bytes).unwrap();
        project.write("plan.json", "existing plan\n");
        for args in [
            vec!["fmt", "--check"],
            vec!["plan", "--no-dev", "--out", "plan.json"],
        ] {
            let human = project.run(&args, 1);
            insta::assert_snapshot!(format!("{}_{case}", args[0]), human.snapshot());
            let mut json_args = args.clone();
            json_args.extend(["--format", "json"]);
            let capture = project.run(&json_args, 1);
            let json = capture.json(args[0], "unanswerable");
            assert_eq!(json["findings"][0]["id"], id);
            assert_eq!(json["findings"][0]["severity"], "error");
            assert!(json["findings"][0]["message"].is_string());
            assert!(json["findings"][0].get("remedy").is_none());
            let summary = match (args[0], case) {
                ("fmt", "io") => {
                    "error: cannot read `./schema/dbo.t.yml`: stream did not contain valid UTF-8\n"
                }
                ("fmt", _) => "error: `./schema/dbo.t.yml` does not parse\n",
                _ => "error: the declarations have 1 problem(s)\n",
            };
            assert_eq!(normalize(&capture.stderr), summary);
            assert_eq!(
                std::fs::read_to_string(project.0.join("plan.json")).unwrap(),
                "existing plan\n"
            );
            assert!(!project.0.join("schema.ids.json").exists());
            assert_eq!(
                std::fs::read(project.0.join("schema/dbo.t.yml")).unwrap(),
                bytes
            );
        }
    }
}

#[test]
fn shared_loader_preserves_each_error_and_the_total() {
    let project = Project::new();
    project.write("schema/z.yml", FK);
    project.write("schema/a.yml", UNICODE);
    project.write("docs.md", "existing docs\n");
    let human = project.run(&["docs", "--out", "docs.md"], 1);
    assert!(
        human
            .stderr
            .ends_with("error: the declarations have 2 problem(s)\n")
    );
    insta::assert_snapshot!("docs_multiple_diagnostics", human.snapshot());
    assert_eq!(
        std::fs::read_to_string(project.0.join("docs.md")).unwrap(),
        "existing docs\n"
    );
}

#[test]
fn multiple_diagnostics_are_ordered_and_complete() {
    let project = Project::new();
    project.write("schema/z.yml", FK);
    project.write("schema/a.yml", UNICODE);
    let human = project.run(&["validate"], 2);
    assert!(human.stderr.find("a.yml").unwrap() < human.stderr.find("z.yml").unwrap());
    insta::assert_snapshot!("multiple_diagnostics", human.snapshot());
    project.write("schema/z.yml", GOOD);
    project.write("schema/a.yml", &GOOD.replace("dbo.t", "dbo.a"));
    project.run(&["validate"], 0);
}
