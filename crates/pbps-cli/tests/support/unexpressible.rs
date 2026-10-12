use super::{Demo, code, stderr, stdout};

fn report(output: std::process::Output) -> serde_json::Value {
    assert_eq!(code(&output), 1, "{}{}", stdout(&output), stderr(&output));
    let report: serde_json::Value = serde_json::from_str(&stdout(&output)).unwrap();
    assert_eq!(report["command"], "plan");
    assert_eq!(report["result"], "unanswerable");
    report
}

/// Compare the actual CLI envelopes and the files a refused plan must preserve.
pub fn check(d: &Demo, connection: &str, objects: &[&str]) {
    let ids_path = d.dir.join("schema.ids.json");
    let ids = std::fs::read(&ids_path).unwrap();
    let offline = report(d.run(&["--no-input", "plan", "--format", "json"]));
    let reasons = offline["findings"].as_array().unwrap();
    assert_eq!(reasons.len(), objects.len(), "{offline}");
    for object in objects {
        assert!(
            reasons.iter().any(|finding| {
                finding["id"] == "change.unexpressible"
                    && finding["message"].as_str().unwrap().contains(object)
            }),
            "missing {object}: {offline}"
        );
    }

    let plan = d.dir.join("refused-plan.json");
    let sql = d.dir.join("refused-plan.sql");
    // Each call starts absent, then proves existing outputs survive too.
    for path in [&plan, &sql] {
        if path.exists() {
            std::fs::remove_file(path).unwrap();
        }
    }
    for existing in [false, true] {
        if existing {
            std::fs::write(&plan, "previous approved plan\n").unwrap();
            std::fs::write(&sql, "-- previous reviewed SQL\n").unwrap();
        }
        let output = d.run(&[
            "--no-input",
            "plan",
            "--db",
            connection,
            "--format",
            "json",
            "--out",
            plan.to_str().unwrap(),
            "--sql",
            sql.to_str().unwrap(),
        ]);
        for reason in reasons {
            assert!(!stderr(&output).contains(reason["message"].as_str().unwrap()));
        }
        let connected = report(output);
        assert_eq!(connected["findings"], offline["findings"], "{connected}");
        if existing {
            assert_eq!(
                std::fs::read_to_string(&plan).unwrap(),
                "previous approved plan\n"
            );
            assert_eq!(
                std::fs::read_to_string(&sql).unwrap(),
                "-- previous reviewed SQL\n"
            );
        } else {
            assert!(!plan.exists() && !sql.exists());
        }
        assert_eq!(std::fs::read(&ids_path).unwrap(), ids);
    }

    let human = d.run(&["--no-input", "plan", "--db", connection]);
    assert_eq!(code(&human), 1, "{}{}", stdout(&human), stderr(&human));
    for reason in reasons {
        let message = reason["message"].as_str().unwrap();
        assert_eq!(
            stderr(&human).matches(message).count(),
            1,
            "{}",
            stderr(&human)
        );
    }
}
