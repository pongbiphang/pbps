use super::Project;

#[test]
fn conflicting_default_and_identity_do_not_invent_an_omitted_value_error() {
    for dialect in ["postgres", "mssql"] {
        for mode in ["exact", "ensure"] {
            let p = Project::new();
            p.write("pbps.yml", &format!("dialect: {dialect}\n"));
            p.write("schema/a.yml", &format!("table: app.t\ncolumns:\n  id: {{type: int, nullable: false}}\n  v: {{type: int, nullable: false, default: '7', identity: [1, 1]}}\nprimary_key: [id]\ndata:\n  mode: {mode}\n  rows:\n    '1': {{}}\n"));
            let capture = p.run(&["validate", "--format", "json"], 2);
            let json: serde_json::Value = serde_json::from_str(&capture.stdout).unwrap();
            let errors: Vec<_> = json["findings"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|f| f["severity"] == "error")
                .collect();
            assert_eq!(errors.len(), 1, "{json}");
            assert_eq!(errors[0]["id"], "dialect.rejected", "{json}");
            let message = errors[0]["message"].as_str().unwrap();
            assert!(
                message.contains("identity") || message.contains("IDENTITY"),
                "{message}"
            );
            assert!(message.contains("default"), "{message}");
        }
    }
}

#[test]
fn source_only_changes_remain_visible_to_since_policy_filtering() {
    for name in ["dbo.OldTable", "dbo.a\nb\\c\u{200b}"] {
        let p = Project::new();
        p.write("pbps.yml", "dialect: mssql\npolicies:\n  rules:\n    naming.table: {severity: error, pattern: '[a-z_]+'}\n");
        let ordinary = "table: dbo.OldTable\ncolumns:\n  id: {type: int, nullable: false}\n"
            .replace("dbo.OldTable", &serde_json::to_string(name).unwrap());
        p.write("schema/a.yml", &ordinary);
        p.run(&["plan"], 0);
        for args in [
            vec!["add", "."],
            vec![
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.invalid",
                "commit",
                "-qm",
                "baseline",
            ],
        ] {
            let result = std::process::Command::new("git")
                .args(args)
                .current_dir(&p.0)
                .output()
                .unwrap();
            assert!(
                result.status.success(),
                "{}",
                String::from_utf8_lossy(&result.stderr)
            );
        }
        p.run(&["validate", "--since", "HEAD", "--format", "json"], 0);
        p.write(
            "schema/a.yml",
            &ordinary.replace(
                "nullable: false",
                "nullable: false, default: '7', identity: [1, 1]",
            ),
        );
        let capture = p.run(&["validate", "--since", "HEAD", "--format", "json"], 2);
        let json: serde_json::Value = serde_json::from_str(&capture.stdout).unwrap();
        assert!(
            json["findings"]
                .as_array()
                .unwrap()
                .iter()
                .any(|f| f["id"] == "naming.table"),
            "{json}"
        );
        // An unchanged ordinary table still receives gradual-adoption filtering.
        p.write("schema/a.yml", &ordinary);
        p.run(&["validate", "--since", "HEAD", "--format", "json"], 0);
    }
}

fn sources(mask: u8) -> String {
    [
        (1, "default: '7'"),
        (2, "identity: [1, 1]"),
        (4, "generated: {expression: 'a * 2', stored: true}"),
    ]
    .into_iter()
    .filter(|(bit, _)| mask & bit != 0)
    .map(|(_, text)| format!(", {text}"))
    .collect()
}

fn messages(p: &Project) -> Vec<String> {
    let capture = p.run(&["validate", "--format", "json"], 2);
    let json: serde_json::Value = serde_json::from_str(&capture.stdout).unwrap();
    json["findings"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["message"].as_str().unwrap().to_owned())
        .collect()
}

#[test]
fn supplied_sources_survive_row_diagnostics_without_hiding_independent_errors() {
    for dialect in ["postgres", "mssql"] {
        for mode in ["exact", "ensure"] {
            for mask in 0..8 {
                for body in ["", ", v: null", ", v: 9"] {
                    let p = Project::new();
                    p.write("pbps.yml", &format!("dialect: {dialect}\n"));
                    p.write("schema/a.yml", &format!(
                        "table: app.t\ncolumns:\n  a: {{type: int, nullable: false}}\n  v: {{type: int, nullable: false{}}}\n  required: {{type: int, nullable: false}}\nprimary_key: [a]\ndata:\n  mode: {mode}\n  rows:\n    '1': {{unknown: 1{body}}}\n", sources(mask)));
                    let got = messages(&p);
                    let has = |text: &str| got.iter().any(|m| m.contains(text));
                    assert!(
                        has("sets `unknown`, which the table does not declare"),
                        "{got:?}"
                    );
                    assert!(has("leaves `required` unset"), "{got:?}");
                    assert_eq!(
                        has("leaves `v` unset"),
                        body.is_empty() && mask == 0,
                        "{dialect}/{mode}/{mask}/{body}: {got:?}"
                    );
                    assert_eq!(has("sets `v` to null"), body.contains("null"), "{got:?}");
                    assert_eq!(
                        has("leave it out to get the default"),
                        body.contains("null") && mask & 1 != 0,
                        "{got:?}"
                    );
                    assert_eq!(
                        has("which is an IDENTITY column"),
                        !body.is_empty() && mask & 2 != 0,
                        "{got:?}"
                    );
                    assert_eq!(
                        has("which is a generated column"),
                        !body.is_empty() && mask & 4 != 0,
                        "{got:?}"
                    );
                }
            }
        }
    }
}

#[test]
fn supplied_sources_survive_key_diagnostics_and_empty_scope_controls() {
    for dialect in ["postgres", "mssql"] {
        for mode in ["exact", "ensure"] {
            for mask in 0..8 {
                for rows in ["{}", "{'1': {}}"] {
                    let p = Project::new();
                    p.write("pbps.yml", &format!("dialect: {dialect}\n"));
                    p.write("schema/a.yml", &format!(
                        "table: app.t\ncolumns:\n  a: {{type: int}}\n  v: {{type: int, nullable: false{}}}\nprimary_key: [v]\ndata:\n  mode: {mode}\n  rows: {rows}\n", sources(mask)));
                    // An unrelated type error keeps every case on the refusal path.
                    p.write(
                        "schema/z.yml",
                        "table: app.z\ncolumns: {bad: {type: no_such_type}}\n",
                    );
                    let got = messages(&p);
                    let has = |text: &str| got.iter().any(|m| m.contains(text));
                    assert_eq!(
                        has("cannot key its rows by `v`, an identity column"),
                        dialect == "postgres" && mask & 2 != 0,
                        "{got:?}"
                    );
                    assert_eq!(
                        has("through their primary-key values"),
                        rows != "{}" && mask & 4 != 0,
                        "{got:?}"
                    );
                    assert!(has("no_such_type"), "{got:?}");
                }
            }
        }
    }
}

#[test]
fn period_checks_keep_identity_context_beside_a_default() {
    for identity in [false, true] {
        let p = Project::new();
        let identity = if identity { ", identity: [1, 1]" } else { "" };
        p.write("schema/a.yml", &format!(
            "table: dbo.t\ncolumns:\n  start: {{type: datetime2, nullable: false, default: SYSUTCDATETIME(){identity}}}\n  end: {{type: datetime2, nullable: false}}\nsystem_time:\n  period: [start, end]\n"));
        p.write(
            "schema/z.yml",
            "table: dbo.z\ncolumns: {bad: {type: no_such_type}}\n",
        );
        let got = messages(&p);
        assert_eq!(
            got.iter().any(|m| m.contains("cannot also be an identity")),
            !identity.is_empty(),
            "{got:?}"
        );
        assert!(got.iter().any(|m| m.contains("no_such_type")), "{got:?}");
    }
}

#[test]
fn partition_checks_keep_all_parent_sources_and_existing_diagnostic_precedence() {
    for mask in 0..8 {
        let p = Project::new();
        p.write("pbps.yml", "dialect: postgres\n");
        p.write("schema/a.yml", &format!(
            "table: app.t\ncolumns:\n  a: {{type: int}}\n  v: {{type: int{}}}\npartition_by: [a]\npartitions:\n  child:\n    from: [0]\n    to: [10]\n    columns:\n      v: {{default: '7'}}\n", sources(mask)));
        p.write(
            "schema/z.yml",
            "table: app.z\ncolumns: {bad: {type: no_such_type}}\n",
        );
        let got = messages(&p);
        let has = |text: &str| got.iter().any(|m| m.contains(text));
        assert_eq!(has("default is its parent's"), mask & 1 != 0, "{got:?}");
        assert_eq!(has("is generated on its parent"), mask & 4 != 0, "{got:?}");
        assert_eq!(
            has("is an identity column on its parent"),
            mask & 2 != 0 && mask & 4 == 0,
            "{got:?}"
        );
        assert!(has("no_such_type"), "{got:?}");
    }
}

#[test]
fn generated_relation_names_keep_identity_context_for_first_choices_and_retries() {
    for mask in [0, 1, 2, 3, 6, 7] {
        for suffix in [0, 1, 2] {
            let p = Project::new();
            p.write("pbps.yml", "dialect: postgres\n");
            // These distinct long names truncate to the same sequence name.
            for (file, ending) in [("a", "x"), ("b", "y")] {
                p.write(&format!("schema/{file}.yml"), &format!(
                    "table: app.{}{ending}\ncolumns:\n  a: {{type: int}}\n  id: {{type: int, nullable: false{}}}\n", "t".repeat(60), sources(mask)));
            }
            let name = if suffix == 0 {
                format!("{}_id_seq", "t".repeat(56))
            } else {
                format!("{}_id_seq{suffix}", "t".repeat(55))
            };
            p.write(
                "schema/z.yml",
                &format!("table: app.{name}\ncolumns: {{bad: {{type: no_such_type}}}}\n"),
            );
            let got = messages(&p);
            assert_eq!(
                got.iter()
                    .any(|m| m.contains("sequence") && m.contains("Rename the other object")),
                mask & 2 != 0 && suffix < 2,
                "mask={mask}, suffix={suffix}: {got:?}"
            );
            assert!(got.iter().any(|m| m.contains("no_such_type")), "{got:?}");
        }
    }
}
