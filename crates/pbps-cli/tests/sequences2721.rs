//! Declared standalone sequences, offline (#2721 PR A, DEC-2721.1): what `validate` accepts and
//! refuses before any database, that `fmt` writes one spelling and is stable, and that SQL
//! Server refuses the file by name. The pull, the census, the counter and the round trip against
//! the engine are the live tests of `flow_pg`.

use std::path::PathBuf;
use std::process::{Command, Output};

const BIN: &str = env!("CARGO_BIN_EXE_pbps");

struct Project(PathBuf);

impl Project {
    fn new(case: &str, dialect: &str) -> Project {
        let root = std::env::temp_dir().join(format!("pbps-seq-{case}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("schema")).unwrap();
        std::fs::write(root.join("pbps.yml"), format!("dialect: {dialect}\n")).unwrap();
        Project(root)
    }

    fn file(&self, name: &str, text: &str) {
        std::fs::write(self.0.join("schema").join(name), text).unwrap();
    }

    fn read(&self, name: &str) -> String {
        std::fs::read_to_string(self.0.join("schema").join(name)).unwrap()
    }

    fn run(&self, command: &[&str]) -> Output {
        Command::new(BIN)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .args(["--no-input", "--project"])
            .arg(&self.0)
            .args(command)
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

const SEQUENCE: &str = "sequence: app.order_no\ntype: integer\nstart: 100\nincrement: 5\n\
                        max: 999999\ncycle: true\ncache: 20\n";

/// A held declaration validates, and so do the engine's other spellings of its type.
#[test]
fn a_valid_sequence_validates_whatever_spelling_its_type_takes() {
    let p = Project::new("valid", "postgres");
    p.file("app.order_no.sequence.yml", SEQUENCE);
    p.file("a.yml", "sequence: app.plain\n");
    p.file("b.yml", "sequence: app.small\ntype: int2\n");
    let out = p.run(&["validate"]);
    assert_eq!(out.status.code(), Some(0), "{}", said(&out));
}

/// Each declaration the engine or this model would refuse is refused by name at `validate`,
/// before any database.
#[test]
fn a_sequence_that_cannot_be_held_is_refused_by_name() {
    for (case, file, expected) in [
        ("type", "sequence: app.s\ntype: numeric\n", "numeric"),
        (
            "zero",
            "sequence: app.s\nincrement: 0\n",
            "INCREMENT must not be zero",
        ),
        (
            "start",
            "sequence: app.s\ntype: smallint\nstart: 40000\n",
            "START value (40000)",
        ),
        (
            "down",
            "sequence: app.s\ntype: integer\nincrement: -1\n",
            "START value (1)",
        ),
        (
            "default-max",
            "sequence: app.s\ntype: smallint\nmax: 32767\n",
            "already the engine's max",
        ),
        (
            "crossed",
            "sequence: app.s\nmin: 10\nmax: 5\nstart: 10\n",
            "minimum must be below its maximum",
        ),
        ("cache", "sequence: app.s\ncache: 1\n", "leave out `cache:`"),
        ("counter", "sequence: app.s\nlast_value: 7\n", "last_value"),
    ] {
        let p = Project::new(case, "postgres");
        p.file("x.yml", file);
        let out = p.run(&["validate"]);
        assert_ne!(out.status.code(), Some(0), "{case}: {}", said(&out));
        let text = said(&out);
        assert!(
            text.contains(expected) && text.contains("app.s"),
            "{case}: {text}"
        );
    }
    // Control: the same shapes with held values validate.
    let p = Project::new("control", "postgres");
    p.file(
        "x.yml",
        "sequence: app.s\ntype: smallint\nmax: 1000\ncache: 5\n",
    );
    assert_eq!(p.run(&["validate"]).status.code(), Some(0));
}

/// A sequence shares the relation namespace with tables and views: one of the same name is
/// refused by name, and the same sequence twice is one declaration twice.
#[test]
fn a_sequence_sharing_a_name_with_a_table_is_refused_and_so_is_one_declared_twice() {
    let p = Project::new("namesake", "postgres");
    p.file("s.yml", "sequence: app.t\n");
    p.file("t.yml", "table: app.t\ncolumns:\n  id: {type: integer}\n");
    let out = p.run(&["validate"]);
    assert_ne!(out.status.code(), Some(0), "{}", said(&out));
    assert!(
        said(&out).contains("shares its name with a table"),
        "{}",
        said(&out)
    );
    // Negative: another name coexists with the table.
    p.file("s.yml", "sequence: app.t_seq\n");
    assert_eq!(p.run(&["validate"]).status.code(), Some(0));
    p.file("u.yml", "sequence: app.t_seq\nstart: 2\n");
    let out = p.run(&["validate"]);
    assert_ne!(out.status.code(), Some(0), "{}", said(&out));
    assert!(said(&out).contains("already declared"), "{}", said(&out));
}

/// `fmt` writes one spelling: type, start and increment always, the options when set. A file
/// that leaves the defaults out is rewritten, and the rewrite is stable.
#[test]
fn fmt_writes_one_spelling_of_a_sequence_and_is_stable() {
    let p = Project::new("fmt", "postgres");
    p.file("s.yml", "sequence: app.s\ncache: 20\n");
    let check = p.run(&["fmt", "--check"]);
    assert_ne!(check.status.code(), Some(0), "{}", said(&check));
    let out = p.run(&["fmt"]);
    assert_eq!(out.status.code(), Some(0), "{}", said(&out));
    assert_eq!(
        p.read("s.yml"),
        "sequence: app.s\ntype: bigint\nstart: 1\nincrement: 1\ncache: 20\n"
    );
    // Negative: nothing left to rewrite, and the file still validates.
    assert_eq!(p.run(&["fmt", "--check"]).status.code(), Some(0));
    assert_eq!(p.run(&["validate"]).status.code(), Some(0));
}

/// SQL Server has sequences, and this tool does not manage them yet: the file is refused by
/// name, with the remedy, and a project with none is unchanged.
#[test]
fn sql_server_refuses_a_sequence_file_by_name() {
    let p = Project::new("mssql", "mssql");
    p.file("x.yml", "sequence: dbo.order_no\n");
    let out = p.run(&["validate"]);
    assert_ne!(out.status.code(), Some(0), "{}", said(&out));
    let text = said(&out);
    assert!(
        text.contains("dbo.order_no") && text.contains("remove the file"),
        "{text}"
    );
    let none = Project::new("mssql-none", "mssql");
    none.file(
        "t.yml",
        "table: dbo.t\ncolumns:\n  id: {type: int, nullable: false}\nprimary_key: [id]\n",
    );
    assert_eq!(none.run(&["validate"]).status.code(), Some(0));
}
