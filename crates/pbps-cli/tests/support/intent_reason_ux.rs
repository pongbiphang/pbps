use super::{Demo, code, stderr, stdout};
use std::io::Write as _;
use std::process::{Command, Output, Stdio};

fn piped(project: &std::path::Path, args: &[&str], input: &[u8]) -> Output {
    let mut child = Command::new(super::BIN)
        .arg("--project")
        .arg(project)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // Bad arguments may be refused before stdin is opened.
    let _ = child.stdin.take().unwrap().write_all(input);
    child.wait_with_output().unwrap()
}

pub fn stdin_reasons(dialect: &str) {
    if dialect == "postgres" {
        partition_reasons();
    }
    let schema = if dialect == "postgres" { "app" } else { "dbo" };
    for command in ["drop", "drop-table", "drop-role"] {
        let d = Demo::new(&format!("stdin2332-{command}"));
        d.table(&format!("table: {schema}.t\ncolumns:\n  id: {{type: int, nullable: false}}\n  old: {{type: int}}\nprimary_key: [id]\n"));
        let role = d.dir.join("schema/reader.yml");
        std::fs::write(&role, "role: reader\n").unwrap();
        assert_eq!(code(&d.run(&["plan", "--no-dev"])), 0);
        d.commit();
        let target = match command {
            "drop" => {
                d.table(&format!("table: {schema}.t\ncolumns:\n  id: {{type: int, nullable: false}}\nprimary_key: [id]\n"));
                format!("{schema}.t.old")
            }
            "drop-table" => {
                std::fs::remove_file(d.dir.join(format!("schema/{schema}.t.yml"))).unwrap();
                format!("{schema}.t")
            }
            "drop-role" => {
                std::fs::remove_file(role).unwrap();
                "reader".into()
            }
            _ => unreachable!(),
        };
        let ids = d.dir.join("schema.ids.json");
        let before = std::fs::read(&ids).unwrap();
        // Both report formats offer an empty reason that cannot record intent.
        let human = d.run(&["plan", "--check"]);
        assert!(
            stderr(&human).contains("--reason \"\""),
            "{}",
            stderr(&human)
        );
        let json = d.run(&["plan", "--check", "--format", "json"]);
        assert!(stdout(&json).contains("--reason \\\"\\\""));
        let unedited = d.run(&[command, &target, "--reason", ""]);
        assert_eq!(code(&unedited), 2);
        assert_eq!(std::fs::read(&ids).unwrap(), before);
        for input in [b"".as_slice(), b" \t\r\n", b"\xff"] {
            let out = piped(&d.dir, &[command, &target, "--reason", "-"], input);
            assert_eq!(code(&out), 2, "{}", stderr(&out));
            assert_eq!(std::fs::read(&ids).unwrap(), before);
        }
        let reason = " \t核准?\n ticket ";
        let out = piped(
            &d.dir,
            &[command, &target, "--reason", "-"],
            format!("{reason}\r\n").as_bytes(),
        );
        assert_eq!(code(&out), 0, "{}{}", stdout(&out), stderr(&out));
        let recorded: pbps_model::IdsFile =
            serde_json::from_slice(&std::fs::read(&ids).unwrap()).unwrap();
        assert!(recorded.tombstones.values().all(|t| t.reason == reason));
        assert!(recorded.tombstones.values().any(|t| t.was == target));
        assert_eq!(
            stderr(&out).contains("Windows PowerShell 5.1"),
            cfg!(windows)
        );
    }
}

pub fn refusal_before_io(dialect: &str) {
    let d = Demo::new("stdin2332-noio");
    let missing = d.dir.join("missing");
    for command in [
        "drop",
        "drop-table",
        "drop-role",
        "move-rows",
        "rebuild-table",
    ] {
        let mut args = vec![command, "app.t", "--reason", "-"];
        if matches!(command, "move-rows" | "rebuild-table") {
            args.extend(["--max-rows", "1"]);
        }
        for input in [b"".as_slice(), b" \t\r\n", b"\xff"] {
            let out = piped(&missing, &args, input);
            assert_eq!(code(&out), 2, "{command}: {}", stderr(&out));
            assert!(stderr(&out).contains("reason"));
            assert!(!stderr(&out).contains("pbps.yml"));
        }
    }
    for value in ["-", "-ticket"] {
        let out = piped(
            &missing,
            &["intent", "--drop", "app.t.old", value],
            b"approved\n",
        );
        assert_eq!(code(&out), 2, "{}", stderr(&out));
        assert!(stderr(&out).contains("single commands"), "{}", stderr(&out));
        assert!(!stderr(&out).contains("pbps.yml"));
    }
    for command in ["plan", "apply"] {
        for flag in ["--reason", "--drop", "--rename", "--intent"] {
            let out = piped(&d.dir, &[command, flag, "-"], b"approved\n");
            assert_eq!(code(&out), 2, "{command} {flag}: {}", stderr(&out));
        }
    }
    // Keep the pipe open: plan --check must finish without waiting for EOF.
    let mut child = Command::new(super::BIN)
        .arg("--project")
        .arg(&d.dir)
        .args(["plan", "--check"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let pipe = child.stdin.take().unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while child.try_wait().unwrap().is_none() {
        if std::time::Instant::now() > deadline {
            child.kill().unwrap();
            let _ = child.wait();
            panic!("plan --check waited for stdin ({dialect})");
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    drop(pipe);
    let out = child.wait_with_output().unwrap();
    assert_eq!(code(&out), 0, "{}{}", stdout(&out), stderr(&out));
    assert!(!missing.exists());
    assert!(!d.dir.join("schema.ids.json").exists());
}

fn partition_reasons() {
    let reason = " \tapproved\n ticket ";
    let d = Demo::new("stdin2332-move");
    d.table("table: app.t\ncolumns:\n  id: {type: int, nullable: false}\nprimary_key: [id]\npartition_by: [id]\npartitions:\n  t_rest: default\n  t_one: {from: ['0'], to: ['10']}\n");
    assert_eq!(code(&d.run(&["plan", "--no-dev"])), 0);
    d.commit();
    let out = piped(
        &d.dir,
        &[
            "move-rows",
            "app.t_one",
            "--max-rows",
            "20",
            "--reason",
            "-",
        ],
        format!("{reason}\n").as_bytes(),
    );
    assert_eq!(code(&out), 0, "{}{}", stdout(&out), stderr(&out));
    let ids: pbps_model::IdsFile =
        serde_json::from_slice(&std::fs::read(d.dir.join("schema.ids.json")).unwrap()).unwrap();
    assert_eq!(ids.moves.values().next().unwrap().reason, reason);

    use pbps_diff::rebuild::member_cases;
    let d = Demo::new("stdin2332-rebuild");
    let root = member_cases::root();
    let declaration = |schema: &pbps_model::Schema| {
        let members: Vec<_> = schema
            .tables
            .iter()
            .filter(|(name, _)| *name != &root)
            .collect();
        pbps_load::render_partitioned(&root, &schema.tables[&root], &members, &[], None)
    };
    let path = d.dir.join("schema/app.p.yml");
    std::fs::write(&path, declaration(&member_cases::source())).unwrap();
    assert_eq!(code(&d.run(&["plan", "--no-dev"])), 0);
    d.commit();
    let case = member_cases::cases()
        .into_iter()
        .find(|case| case.refusal.is_none())
        .unwrap();
    std::fs::write(path, declaration(&case.desired)).unwrap();
    let out = piped(
        &d.dir,
        &[
            "rebuild-table",
            "app.p",
            "--max-rows",
            "20",
            "--reason",
            "-",
        ],
        format!("{reason}\r\n").as_bytes(),
    );
    assert_eq!(code(&out), 0, "{}{}", stdout(&out), stderr(&out));
    let ids: pbps_model::IdsFile =
        serde_json::from_slice(&std::fs::read(d.dir.join("schema.ids.json")).unwrap()).unwrap();
    assert_eq!(
        ids.rebuilds.values().next().unwrap().reason.as_ref(),
        reason
    );
}
