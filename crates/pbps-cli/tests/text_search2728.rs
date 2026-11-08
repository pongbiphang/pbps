//! Declared text search configurations and dictionaries, offline (#2728 PR A, DEC-2728.1):
//! what `validate` accepts and refuses before any database, and that SQL Server refuses
//! the file by name. The pull, the census and the round trip against the engine are the
//! live tests of `flow_pg`.

use std::path::PathBuf;
use std::process::{Command, Output};

const BIN: &str = env!("CARGO_BIN_EXE_pbps");

struct Project(PathBuf);

impl Project {
    fn new(case: &str, dialect: &str) -> Project {
        let root = std::env::temp_dir().join(format!("pbps-search-{case}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("schema")).unwrap();
        std::fs::write(root.join("pbps.yml"), format!("dialect: {dialect}\n")).unwrap();
        Project(root)
    }

    fn file(&self, name: &str, text: &str) {
        std::fs::write(self.0.join("schema").join(name), text).unwrap();
    }

    fn validate(&self) -> Output {
        Command::new(BIN)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .args(["--no-input", "--project"])
            .arg(&self.0)
            .arg("validate")
            .output()
            .unwrap()
    }
}

impl Drop for Project {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn said(out: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

const DICTIONARY: &str = "text_search_dictionary: app.russian\ntemplate: pg_catalog.snowball\n\
                          options:\n  language: russian\n";
const CONFIG: &str = "text_search_config: app.russian\nparser: pg_catalog.\"default\"\n\
                      mappings:\n  asciiword: [app.russian, pg_catalog.simple]\n  email: [pg_catalog.simple]\n";

/// A configuration and a dictionary of one name, and a domain of it, are three
/// declarations that coexist: `validate` accepts the project.
#[test]
fn a_configuration_a_dictionary_and_a_domain_of_one_name_coexist() {
    let p = Project::new("coexist", "postgres");
    p.file("app.russian.textsearch-dictionary.yml", DICTIONARY);
    p.file("app.russian.textsearch-config.yml", CONFIG);
    p.file("app.russian.type.yml", "domain: app.russian\ntype: text\n");
    let out = p.validate();
    assert_eq!(out.status.code(), Some(0), "{}", said(&out));
    // Negative: two files of one kind and name are one declaration twice.
    p.file("app.russian2.textsearch-dictionary.yml", DICTIONARY);
    let out = p.validate();
    assert_ne!(out.status.code(), Some(0), "{}", said(&out));
    assert!(said(&out).contains("already declared"), "{}", said(&out));
}

/// Each declaration the engine or this model would refuse is refused by name at `validate`,
/// before any database.
#[test]
fn a_text_search_declaration_that_cannot_be_held_is_refused_by_name() {
    for (case, file, expected) in [
        (
            "parser",
            "text_search_config: app.c\nparser: ext.myparser\n",
            "built-in one",
        ),
        (
            "alias",
            "text_search_config: app.c\nparser: pg_catalog.\"default\"\nmappings:\n  wurd: [pg_catalog.simple]\n",
            "not a token type",
        ),
        (
            "repeat",
            "text_search_config: app.c\nparser: pg_catalog.\"default\"\nmappings:\n  word: [pg_catalog.simple, pg_catalog.simple]\n",
            "twice",
        ),
        (
            "template",
            "text_search_dictionary: app.d\ntemplate: ext.mytemplate\n",
            "built-in ones",
        ),
    ] {
        let p = Project::new(case, "postgres");
        p.file("x.yml", file);
        let out = p.validate();
        assert_ne!(out.status.code(), Some(0), "{case}: {}", said(&out));
        assert!(said(&out).contains(expected), "{case}: {}", said(&out));
    }
    // Control: the same shapes with held values validate.
    let p = Project::new("control", "postgres");
    p.file("x.yml", CONFIG);
    p.file("y.yml", DICTIONARY);
    assert_eq!(p.validate().status.code(), Some(0));
}

/// SQL Server has no equivalent: either kind is refused by name, with the remedy, and a
/// project with none is unchanged.
#[test]
fn sql_server_refuses_either_text_search_file_by_name() {
    for (case, file, key) in [
        ("config", CONFIG, "text_search_config"),
        ("dictionary", DICTIONARY, "text_search_dictionary"),
    ] {
        let p = Project::new(case, "mssql");
        p.file("x.yml", file);
        let out = p.validate();
        assert_ne!(out.status.code(), Some(0), "{case}: {}", said(&out));
        let text = said(&out);
        assert!(
            text.contains(key) && text.contains("remove the file"),
            "{case}: {text}"
        );
    }
    let none = Project::new("none", "mssql");
    none.file(
        "t.yml",
        "table: dbo.t\ncolumns:\n  id: {type: int, nullable: false}\nprimary_key: [id]\n",
    );
    let out = none.validate();
    assert_eq!(out.status.code(), Some(0), "{}", said(&out));
}
