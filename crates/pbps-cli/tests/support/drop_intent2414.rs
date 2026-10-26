//! Tracked removals require recorded reasons even when the checkout lost their UIDs.
use super::*;
use std::collections::BTreeMap;

fn succeeds(output: std::process::Output) -> std::process::Output {
    assert_eq!(code(&output), 0, "{}{}", stdout(&output), stderr(&output));
    output
}

struct ClusterRole {
    server: String,
    name: String,
}
impl Drop for ClusterRole {
    fn drop(&mut self) {
        let _ = super::try_on_server(&self.server, &format!("DROP ROLE \"{}\"", self.name));
    }
}

struct Removal {
    file: &'static str,
    line: Option<&'static str>,
    section: &'static str,
    name: String,
    message: String,
    id: &'static str,
    command: Vec<String>,
}

const GENERATED_COLUMN: &str = concat!(
    "  g: {type: bigint, generated: {expression: id + 1, ",
    "stored: true}}\n",
);

pub(super) fn check(server: &str, postgres: bool) {
    let schema = if postgres { "app" } else { "dbo" };
    let role = format!("reader2414_{}", std::process::id());
    // A PostgreSQL role is provisioned outside this database. Its guard is
    // created first so the database (and its grants) disappears before the role.
    let _role = postgres.then(|| {
        super::on_server(server, &format!("CREATE ROLE \"{role}\""));
        ClusterRole {
            server: server.to_owned(),
            name: role.clone(),
        }
    });
    let own = OwnDatabase::new(server, "drop_intent2414");
    let connection = own.connection();
    if postgres {
        super::on_server(connection, "CREATE SCHEMA app");
    }
    let d = Demo::new("drop-intent2414");
    let directory = d.dir.join("schema");
    let mut files = BTreeMap::from([
        (
            "keep.yml",
            format!(
                concat!(
                    "table: {schema}.keep\ncolumns:\n",
                    "  id: {{type: bigint, nullable: false}}\n",
                    "  gone: {{type: bigint}}\nchecks:\n  ck_id: id >= 0\n",
                    "indexes:\n  ix_id: {{columns: [id]}}\n",
                ),
                schema = schema
            ),
        ),
        (
            "victim.yml",
            format!("table: {schema}.victim\ncolumns:\n  id: {{type: bigint}}\n"),
        ),
        (
            "role.yml",
            format!(
                "role: {role}\ngrants:\n  {schema}.keep: [select]\n{}",
                if postgres {
                    "  schema::app: [usage]\n"
                } else {
                    ""
                }
            ),
        ),
        (
            "view.yml",
            format!("view: {schema}.v\ndefinition: |\n  SELECT id FROM {schema}.keep\n"),
        ),
    ]);
    if postgres {
        files.insert("enum.yml", "enum: app.mood\nvalues: [sad, happy]\n".into());
        files.insert("domain.yml", "domain: app.count\ntype: bigint\n".into());
        files.insert(
            "generated.yml",
            format!("table: app.generated\ncolumns:\n  id: {{type: bigint}}\n{GENERATED_COLUMN}"),
        );
        files.insert(
            "partitioned.yml",
            concat!(
                "table: app.part\ncolumns:\n  k: {type: bigint}\n",
                "partition_by: [k]\npartitions:\n",
                "  p1: {from: [0], to: [10]}\n  p2: {from: [10], to: [20]}\n",
            )
            .into(),
        );
    }
    for (name, text) in &files {
        std::fs::write(directory.join(name), text).unwrap();
    }
    succeeds(d.run(&["plan", "--no-dev"]));
    d.commit();
    let output = Command::new("git")
        .arg("-C")
        .arg(&d.dir)
        .args(["rev-parse", "HEAD"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let baseline = String::from_utf8(output.stdout).unwrap();
    succeeds(d.run(&["bootstrap", "--db", connection]));
    let ids_path = d.dir.join("schema.ids.json");
    let ids = std::fs::read(&ids_path).unwrap();
    let restore = || {
        for (name, text) in &files {
            std::fs::write(directory.join(name), text).unwrap();
        }
        std::fs::write(&ids_path, &ids).unwrap();
    };
    let column = format!("{schema}.keep.gone");
    let victim = format!("{schema}.victim");
    let mut cases = vec![
        Removal {
            file: "keep.yml",
            line: Some("  gone: {type: bigint}\n"),
            section: "columns",
            name: column.clone(),
            message: format!(
                "{column} disappeared from the declarations, but a drop must record why (an audit asks for it)"
            ),
            id: "identity.drop-column-needs-reason",
            command: vec!["drop".into(), column, "--reason".into(), "retired".into()],
        },
        Removal {
            file: "victim.yml",
            line: None,
            section: "tables",
            name: victim.clone(),
            message: format!(
                "table {victim} disappeared from the declarations, but a drop must record why"
            ),
            id: "identity.drop-table-needs-reason",
            command: vec![
                "drop-table".into(),
                victim,
                "--reason".into(),
                "retired".into(),
            ],
        },
        Removal {
            file: "role.yml",
            line: None,
            section: "roles",
            name: role.clone(),
            message: format!(
                "role {role} disappeared from the declarations, but a drop must record why — its members lose whatever it granted"
            ),
            id: "identity.drop-role-needs-reason",
            command: vec![
                "drop-role".into(),
                role.clone(),
                "--reason".into(),
                "retired".into(),
            ],
        },
    ];
    if postgres {
        for (file, name) in [("enum.yml", "app.mood"), ("domain.yml", "app.count")] {
            let message = format!(
                "type {name} disappeared; record its reason with pbps intent --drop-type {name} \"\""
            );
            cases.push(Removal {
                file,
                line: None,
                section: "types",
                name: name.into(),
                message,
                id: "identity.drop-type-needs-reason",
                command: vec![
                    "intent".into(),
                    "--drop-type".into(),
                    name.into(),
                    "retired".into(),
                ],
            });
        }
        let message = concat!(
            "app.generated.g disappeared from the declarations, but a drop ",
            "must record why (an audit asks for it)"
        );
        cases.push(Removal {
            file: "generated.yml",
            line: Some(GENERATED_COLUMN),
            section: "columns",
            name: "app.generated.g".into(),
            message: message.into(),
            id: "identity.drop-column-needs-reason",
            command: vec![
                "drop".into(),
                "app.generated.g".into(),
                "--reason".into(),
                "retired".into(),
            ],
        });
        cases.push(Removal {
            file: "partitioned.yml",
            line: Some("  p2: {from: [10], to: [20]}\n"),
            section: "tables",
            name: "app.p2".into(),
            message: "table app.p2 disappeared from the declarations, but a drop must record why"
                .into(),
            id: "identity.drop-table-needs-reason",
            command: vec![
                "drop-table".into(),
                "app.p2".into(),
                "--reason".into(),
                "retired".into(),
            ],
        });
    }
    let remove = |case: &Removal| {
        let file = directory.join(case.file);
        if let Some(line) = case.line {
            assert!(files[case.file].contains(line));
            std::fs::write(file, files[case.file].replace(line, "")).unwrap();
        } else {
            std::fs::remove_file(file).unwrap();
        }
    };
    for case in &cases {
        for missing_uid in [false, true] {
            restore();
            remove(case);
            if missing_uid {
                let mut local: serde_json::Value = serde_json::from_slice(&ids).unwrap();
                let map = local[case.section].as_object_mut().unwrap();
                let uid = map
                    .iter()
                    .find(|(_, name)| name.as_str() == Some(case.name.as_str()))
                    .unwrap()
                    .0
                    .clone();
                map.remove(&uid);
                if case.section == "tables" {
                    let prefix = format!("{}.", case.name);
                    local["columns"]
                        .as_object_mut()
                        .unwrap()
                        .retain(|_, name| !name.as_str().unwrap().starts_with(&prefix));
                }
                std::fs::write(&ids_path, serde_json::to_vec_pretty(&local).unwrap()).unwrap();
                let before = std::fs::read(&ids_path).unwrap();
                let args: Vec<_> = case.command.iter().map(String::as_str).collect();
                let output = d.run(&args);
                assert_eq!(
                    code(&output),
                    2,
                    "{}: {}{}",
                    case.name,
                    stdout(&output),
                    stderr(&output)
                );
                let unused = concat!(
                    "this intent matches nothing in either the declarations ",
                    "or the identity file, likely a typo:"
                );
                assert!(stderr(&output).contains(unused), "{}", stderr(&output));
                assert_eq!(std::fs::read(&ids_path).unwrap(), before);
            }
            d.commit();
            let before = std::fs::read(&ids_path).unwrap();
            for connected in [false, true] {
                for format in ["human", "json"] {
                    let artifact = d.dir.join("refused.plan.json");
                    let script = d.dir.join("refused.plan.sql");
                    std::fs::write(&artifact, "existing plan\n").unwrap();
                    std::fs::write(&script, "existing script\n").unwrap();
                    let mut args = vec![
                        "plan",
                        "--out",
                        artifact.to_str().unwrap(),
                        "--sql",
                        script.to_str().unwrap(),
                        "--format",
                        format,
                    ];
                    if connected {
                        args.extend(["--db", connection]);
                    } else {
                        args.extend(["--no-dev", "--since", baseline.trim()]);
                    }
                    let output = d.run(&args);
                    // Keep the existing connected human diagnostic's exit
                    // status; JSON and offline refusals use the finding status.
                    let expected = if connected && format == "human" { 1 } else { 2 };
                    assert_eq!(
                        code(&output),
                        expected,
                        "{} (missing UID {missing_uid}, connected {connected}): {}{}",
                        case.name,
                        stdout(&output),
                        stderr(&output)
                    );
                    if format == "json" {
                        let report: serde_json::Value =
                            serde_json::from_slice(&output.stdout).unwrap();
                        let finding = report["findings"]
                            .as_array()
                            .unwrap()
                            .iter()
                            .find(|f| f["id"] == case.id)
                            .unwrap();
                        assert_eq!(finding["message"], case.message);
                        if case.section != "types" {
                            let mut remedy = case.command[..case.command.len() - 1].join(" ");
                            remedy.push_str(" \"\"");
                            assert_eq!(finding["remedy"], format!("pbps {remedy}"));
                        }
                    } else {
                        assert!(
                            stderr(&output).contains(&case.message),
                            "{}",
                            stderr(&output)
                        );
                    }
                    assert_eq!(std::fs::read(&ids_path).unwrap(), before);
                    assert_eq!(std::fs::read(&artifact).unwrap(), b"existing plan\n");
                    assert_eq!(std::fs::read(&script).unwrap(), b"existing script\n");
                }
            }
        }
    }
    // The intent requirement does not depend on a Git checkout being present.
    let git = d.dir.join(".git");
    let parked = d.dir.join("measurement-git");
    std::fs::rename(&git, &parked).unwrap();
    let output = d.run(&["plan", "--db", connection, "--format", "json"]);
    std::fs::rename(&parked, &git).unwrap();
    assert_eq!(code(&output), 2, "{}{}", stdout(&output), stderr(&output));
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(
        report["findings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|finding| finding["id"] == cases.last().unwrap().id)
    );
    // Definition-only removals retain their current intent boundary.
    restore();
    std::fs::remove_file(directory.join("view.yml")).unwrap();
    std::fs::write(
        directory.join("keep.yml"),
        files["keep.yml"].replace(
            "checks:\n  ck_id: id >= 0\nindexes:\n  ix_id: {columns: [id]}\n",
            "",
        ),
    )
    .unwrap();
    succeeds(d.run(&["plan", "--db", connection]));
    // New columns, attribute changes and an explicit rename keep their admission.
    restore();
    std::fs::write(
        directory.join("keep.yml"),
        files["keep.yml"].replace(
            "  gone: {type: bigint}\n",
            "  gone: {type: bigint, nullable: false, default: 1}\n  added: {type: bigint}\n",
        ),
    )
    .unwrap();
    succeeds(d.run(&["plan", "--no-dev", "--since", baseline.trim()]));
    succeeds(d.run(&["plan", "--db", connection]));
    restore();
    std::fs::write(
        directory.join("keep.yml"),
        files["keep.yml"].replace("  gone: {type: bigint}\n", "  renamed: {type: bigint}\n"),
    )
    .unwrap();
    succeeds(d.run(&["rename", &format!("{schema}.keep.gone"), "renamed"]));
    succeeds(d.run(&["plan", "--db", connection]));
    // Recording the exact baseline UIDs makes all the same removals applyable.
    restore();
    let mut batch = vec!["intent".to_owned()];
    for case in &cases {
        remove(case);
        let flag = match case.section {
            "columns" => "--drop",
            "tables" => "--drop-table",
            "roles" => "--drop-role",
            "types" => "--drop-type",
            _ => unreachable!(),
        };
        batch.extend([flag.into(), case.name.clone(), "retired".into()]);
    }
    let args: Vec<_> = batch.iter().map(String::as_str).collect();
    succeeds(d.run(&args));
    d.commit();
    let plan = d.dir.join("approved.plan.json");
    succeeds(d.run(&["plan", "--db", connection, "--out", plan.to_str().unwrap()]));
    let checksum = super::plan_checksum(&plan);
    succeeds(d.run(&[
        "apply",
        "--db",
        connection,
        "--plan",
        plan.to_str().unwrap(),
        "--checksum",
        &checksum,
        "--allow",
        "destructive,revoke",
    ]));
    let output = succeeds(d.run(&["plan", "--db", connection, "--format", "json"]));
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["data"]["changes"], 0);
    let after = std::fs::read(&ids_path).unwrap();
    for case in &cases {
        let args: Vec<_> = case.command.iter().map(String::as_str).collect();
        let output = succeeds(d.run(&args));
        assert!(stdout(&output).contains("these intents may have taken effect already"));
        assert_eq!(std::fs::read(&ids_path).unwrap(), after);
    }
    let output = d.run(&[
        "drop",
        &format!("{schema}.keep.never_declared"),
        "--reason",
        "retired",
    ]);
    assert_eq!(code(&output), 2, "{}{}", stdout(&output), stderr(&output));

    // NULL deployed provenance also supplies no exception to intent. Establish
    // it through a real bootstrap without Git, not an edited ledger row.
    let unknown_git = OwnDatabase::new(server, "drop_intent2414_null_git");
    if postgres {
        super::on_server(unknown_git.connection(), "CREATE SCHEMA app");
    }
    let plain = Demo::new("drop-intent2414-null-git");
    std::fs::remove_dir_all(plain.dir.join(".git")).unwrap();
    let table = plain.dir.join("schema/plain.yml");
    std::fs::write(
        &table,
        format!(
            concat!(
                "table: {schema}.plain\ncolumns:\n",
                "  id: {{type: bigint}}\n  gone: {{type: bigint}}\n",
            ),
            schema = schema
        ),
    )
    .unwrap();
    succeeds(plain.run(&["plan", "--no-dev"]));
    succeeds(plain.run(&["bootstrap", "--db", unknown_git.connection()]));
    let shown = succeeds(plain.run(&[
        "state",
        "show",
        "1",
        "--db",
        unknown_git.connection(),
        "--format",
        "json",
    ]));
    let report: serde_json::Value = serde_json::from_slice(&shown.stdout).unwrap();
    let snapshot: pbps_model::StateSnapshot =
        serde_json::from_value(report["data"]["entry"]["snapshot"].clone()).unwrap();
    assert!(snapshot.git_sha.is_none());
    let text = std::fs::read_to_string(&table).unwrap();
    std::fs::write(table, text.replace("  gone: {type: bigint}\n", "")).unwrap();
    let path = plain.dir.join("schema.ids.json");
    let mut ids: pbps_model::IdsFile =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    let column = format!("{schema}.plain.gone").parse().unwrap();
    let uid = ids.column_uid(&column).unwrap().clone();
    ids.columns.remove(&uid);
    let before = serde_json::to_vec_pretty(&ids).unwrap();
    std::fs::write(&path, &before).unwrap();
    let output = plain.run(&["plan", "--db", unknown_git.connection(), "--format", "json"]);
    assert_eq!(code(&output), 2, "{}{}", stdout(&output), stderr(&output));
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(
        report["findings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|finding| finding["id"] == "identity.drop-column-needs-reason")
    );
    assert_eq!(std::fs::read(&path).unwrap(), before);
}
