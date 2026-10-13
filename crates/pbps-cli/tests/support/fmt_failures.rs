use super::{Demo, code, stderr, stdout};
use std::path::Path;

fn table(dialect: &str, name: &str) -> String {
    let schema = if dialect == "postgres" { "app" } else { "dbo" };
    format!("table: {schema}.{name}\ncolumns: {{id: {{type: BIGINT, nullable: false}}}}\n")
}

pub fn before_writes(dialect: &str) {
    for (kind, bad) in [("yaml", b"columns: [\n".as_slice()), ("io", &[0xff])] {
        for format in ["human", "json"] {
            for check in [false, true] {
                let d = Demo::new(&format!("fmt-stage-{kind}-{format}-{check}"));
                let first = d.dir.join("schema").join("a.yml");
                let last = d.dir.join("schema").join("z.yml");
                let original = table(dialect, "a");
                std::fs::write(&first, &original).unwrap();
                std::fs::write(&last, bad).unwrap();
                let mut args = vec!["fmt", "--format", format];
                if check {
                    args.push("--check");
                }
                let out = d.run(&args);
                assert_eq!(code(&out), 1, "{}{}", stdout(&out), stderr(&out));
                assert_eq!(std::fs::read(&first).unwrap(), original.as_bytes());
                assert_eq!(std::fs::read(&last).unwrap(), bad);
                assert!(!d.dir.join("schema.ids.json").exists());
                if format == "json" {
                    let report: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
                    assert_eq!(report["command"], "fmt");
                    assert_eq!(report["result"], "unanswerable");
                    let findings = report["findings"].as_array().unwrap();
                    assert_eq!(findings.len(), 1, "{report}");
                    assert_eq!(findings[0]["id"], format!("load.{kind}"));
                    if kind == "io" {
                        assert!(findings[0]["message"].as_str().unwrap().contains("UTF-8"));
                    }
                    assert_eq!(
                        Path::new(findings[0]["location"]["file"].as_str().unwrap()),
                        last
                    );
                } else {
                    assert!(stderr(&out).contains("z.yml"), "{}", stderr(&out));
                    assert!(!stdout(&out).contains("rewrote"), "{}", stdout(&out));
                }

                // A repaired input permits both rewrites, and the next check
                // is clean; staging must not turn a refusal into a stuck state.
                std::fs::write(&last, table(dialect, "z")).unwrap();
                let out = d.run(&["fmt", "--format", "json"]);
                assert_eq!(code(&out), 0, "{}{}", stdout(&out), stderr(&out));
                let report: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
                assert_eq!(report["data"]["changed"], 2);
                assert_eq!(report["data"]["checked"], 2);
                assert_eq!(code(&d.run(&["fmt", "--check"])), 0);
            }
        }
    }
}

pub fn all_errors_and_warnings(dialect: &str) {
    for format in ["human", "json"] {
        let d = Demo::new(&format!("fmt-all-errors-{format}"));
        let first = d.dir.join("schema").join("a.yml");
        let original = table(dialect, "a");
        std::fs::write(&first, &original).unwrap();
        let bad = [
            ("x.yml", b"columns: [\n".as_slice(), "load.yaml"),
            ("y.yml", b"role: [\n".as_slice(), "load.yaml"),
            ("z.yml", &[0xff], "load.io"),
        ];
        for (name, bytes, _) in bad {
            std::fs::write(d.dir.join("schema").join(name), bytes).unwrap();
        }
        let ids = d.dir.join("schema.ids.json");
        std::fs::write(&ids, "broken identity file").unwrap();
        let out = d.run(&["fmt", "--format", format]);
        assert_eq!(code(&out), 1, "{}{}", stdout(&out), stderr(&out));
        if format == "json" {
            let report: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
            assert_eq!(report["result"], "unanswerable");
            let findings = report["findings"].as_array().unwrap();
            assert_eq!(findings.len(), 4, "{report}");
            for (name, _, id) in bad {
                let path = d.dir.join("schema").join(name);
                assert!(
                    findings.iter().any(|f| {
                        f["id"] == id && Path::new(f["location"]["file"].as_str().unwrap()) == path
                    }),
                    "{report}"
                );
            }
            assert!(
                findings
                    .iter()
                    .any(|f| { f["id"] == "identity.unreadable" && f["severity"] == "warning" }),
                "{report}"
            );
        } else {
            let text = stderr(&out);
            for name in ["x.yml", "y.yml", "z.yml", "schema.ids.json", "warning:"] {
                assert!(text.contains(name), "missing {name}: {text}");
            }
            assert!(!stdout(&out).contains("rewrote"));
        }
        assert_eq!(std::fs::read(&first).unwrap(), original.as_bytes());
        for (name, bytes, _) in bad {
            assert_eq!(
                std::fs::read(d.dir.join("schema").join(name)).unwrap(),
                bytes
            );
        }
        assert_eq!(
            std::fs::read_to_string(ids).unwrap(),
            "broken identity file"
        );
    }
}
