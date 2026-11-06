//! Offline CLI coverage for Markdown table-cell boundaries (#1931).
#[path = "support/idtree.rs"]
mod idtree;

use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

struct Project(PathBuf);

impl Project {
    fn new(dialect: &str, schema: &str, name: &str) -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let root = loop {
            let path = std::env::temp_dir().join(format!(
                "issue-1931-docs-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            match std::fs::create_dir(&path) {
                Ok(()) => break path,
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => panic!("create project: {error}"),
            }
        };
        let project = Self(root);
        std::fs::create_dir(project.0.join("schema")).unwrap();
        project.write("pbps.yml", &format!("dialect: {dialect}\n"));
        let table = serde_json::to_string(&format!("{schema}.{name}")).unwrap();
        project.write(
            "schema/table.yml",
            &format!("table: {table}\ncolumns:\n  id: {{type: bigint, nullable: false}}\n"),
        );
        let permission = if dialect == "postgres" {
            "usage"
        } else {
            "select"
        };
        let schema_target = serde_json::to_string(&format!("schema::{schema}")).unwrap();
        project.write(
            "schema/role.yml",
            &format!("role: retail_reporter\ngrants:\n  {table}: [select]\n  {schema_target}: [{permission}]\n"),
        );
        project
    }

    fn write(&self, path: &str, text: &str) {
        std::fs::write(self.0.join(path), text).unwrap();
    }

    fn run(&self, arguments: &[&str]) -> String {
        let output = Command::new(env!("CARGO_BIN_EXE_pbps"))
            .arg("--project")
            .arg(&self.0)
            .args(arguments)
            .output()
            .unwrap();
        assert!(output.status.success(), "{arguments:?}: {output:?}");
        String::from_utf8(output.stdout).unwrap()
    }
}

impl Drop for Project {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).unwrap();
    }
}

#[test]
fn pipe_grant_targets_validate_and_remain_in_one_markdown_cell_on_both_dialects() {
    for (dialect, schema) in [("mssql", "dbo"), ("postgres", "public")] {
        let project = Project::new(dialect, schema, "sales|report");
        project.run(&["validate"]);
        let out = project.run(&["docs", "--format", "markdown"]);
        assert!(
            out.contains(&format!("| `{schema}.sales\\|report` | select |\n")),
            "{out}"
        );
        // File output and stdout use exactly the same renderer.
        project.run(&[
            "docs",
            "--format",
            "markdown",
            "--out",
            project.0.join("schema.md").to_str().unwrap(),
        ]);
        assert_eq!(
            std::fs::read_to_string(project.0.join("schema.md")).unwrap(),
            out
        );
    }
}

#[test]
fn code_fences_and_line_endings_are_safe_after_loading_both_dialects() {
    for (dialect, schema) in [("mssql", "dbo"), ("postgres", "public")] {
        let project = Project::new(dialect, schema, "sales|`報表\r\nnext");
        project.run(&["validate"]);
        let out = project.run(&["docs", "--format", "markdown"]);
        assert!(
            out.contains(&format!("| ``{schema}.sales\\|`報表 next`` | select |\n")),
            "{out}"
        );
    }
    for dialect in ["mssql", "postgres"] {
        let project = Project::new(dialect, "報|`表", "sales");
        project.run(&["validate"]);
        let out = project.run(&["docs", "--format", "markdown"]);
        assert!(out.contains("| ``報\\|`表.sales`` | select |\n"), "{out}");
        let permission = if dialect == "postgres" {
            "usage"
        } else {
            "select"
        };
        assert!(
            out.contains(&format!("| ``schema::報\\|`表`` | {permission} |\n")),
            "{out}"
        );
    }
}

#[test]
fn loaded_descriptions_deprecations_and_tombstones_keep_their_cells() {
    for (dialect, schema) in [("mssql", "dbo"), ("postgres", "public")] {
        let project = Project::new(dialect, schema, "sales");
        let value = "first|`second\n</td><tr>third";
        let quoted = serde_json::to_string(value).unwrap();
        project.write("schema/table.yml", &format!(
            "table: {schema}.sales\ncolumns:\n  id: {{type: bigint, nullable: false, description: {quoted}, deprecated: {quoted}}}\n"
        ));
        let mut ids = pbps_model::IdsFile::default();
        ids.tombstones.insert(
            "c_k7x2mq".parse().unwrap(),
            pbps_model::Tombstone {
                was: value.into(),
                dropped_at: value.into(),
                operator: value.into(),
                reason: value.into(),
                require: None,
            },
        );
        ids.tables.insert(
            "t_aaaaaa".parse().unwrap(),
            format!("{schema}.sales").parse().unwrap(),
        );
        crate::idtree::write(&project.0, &ids);
        project.run(&["validate"]);
        let out = project.run(&["docs", "--format", "markdown"]);
        let plain = "first\\|`second &lt;/td>&lt;tr>third";
        for row in [
            format!("| `id` ~~deprecated~~ | `bigint` | no | — | {plain} |\n"),
            format!("| `{schema}.sales.id` | {plain} |\n"),
            format!("| ``first\\|`second </td><tr>third`` | {plain} | {plain} | {plain} |\n"),
        ] {
            assert!(out.contains(&row), "missing {row:?}: {out}");
        }
    }
}

#[test]
fn loaded_prose_preserves_comparisons_and_escapes_html_openers_on_both_dialects() {
    for (dialect, schema) in [("mssql", "dbo"), ("postgres", "public")] {
        for (authored, escaped) in [
            (
                "Use `amount < 10 | mask` and a < b",
                "Use `amount < 10 \\| mask` and a < b",
            ),
            (
                "[link](url`) </td></tr><tr><td>tail `",
                "[link](url`) &lt;/td>&lt;/tr>&lt;tr>&lt;td>tail `",
            ),
            (
                "`<x` | <!-- note --> <?pi?>",
                "`&lt;x` \\| &lt;!-- note --> &lt;?pi?>",
            ),
        ] {
            for newline in ["\n", "\r\n"] {
                let project = Project::new(dialect, schema, "sales");
                let value = format!("{authored}{newline}</td></tr><tr><td>tail");
                let quoted = serde_json::to_string(&value).unwrap();
                project.write("schema/table.yml", &format!(
                    "table: {schema}.sales\ncolumns:\n  id: {{type: bigint, nullable: false, description: {quoted}, deprecated: {quoted}}}\n"
                ));
                let mut ids = pbps_model::IdsFile::default();
                ids.tombstones.insert(
                    "c_k7x2mq".parse().unwrap(),
                    pbps_model::Tombstone {
                        was: format!("{schema}.sales.old"),
                        dropped_at: value.clone(),
                        operator: value.clone(),
                        reason: value,
                        require: None,
                    },
                );
                ids.tables.insert(
                    "t_aaaaaa".parse().unwrap(),
                    format!("{schema}.sales").parse().unwrap(),
                );
                crate::idtree::write(&project.0, &ids);
                project.run(&["validate"]);
                let out = project.run(&["docs", "--format", "markdown"]);
                let plain = format!("{escaped} &lt;/td>&lt;/tr>&lt;tr>&lt;td>tail");
                for row in [
                    format!("| `id` ~~deprecated~~ | `bigint` | no | — | {plain} |\n"),
                    format!("| `{schema}.sales.id` | {plain} |\n"),
                    format!("| `{schema}.sales.old` | {plain} | {plain} | {plain} |\n"),
                ] {
                    assert!(out.contains(&row), "missing {row:?}: {out}");
                }
                assert!(!out.contains("amount &lt; 10"), "{out}");
                project.run(&[
                    "docs",
                    "--format",
                    "markdown",
                    "--out",
                    project.0.join("schema.md").to_str().unwrap(),
                ]);
                assert_eq!(
                    std::fs::read_to_string(project.0.join("schema.md")).unwrap(),
                    out
                );
            }
        }
    }
}

#[test]
fn ordinary_grants_empty_roles_and_no_roles_keep_their_markdown_bytes() {
    for (dialect, schema) in [("mssql", "dbo"), ("postgres", "public")] {
        let project = Project::new(dialect, schema, "sales");
        let out = project.run(&["docs", "--format", "markdown"]);
        assert!(
            out.contains(&format!("| `{schema}.sales` | select |\n")),
            "{out}"
        );
        project.write("schema/role.yml", "role: pending\ngrants: {}\n");
        let out = project.run(&["docs", "--format", "markdown"]);
        assert!(out.contains("### `pending`\n\nNo grants.\n"), "{out}");
        assert!(!out.contains("| Granted on |"));
        std::fs::remove_file(project.0.join("schema/role.yml")).unwrap();
        let out = project.run(&["docs", "--format", "markdown"]);
        assert!(!out.contains("## Roles"));
    }
}
