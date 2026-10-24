use super::{BIN, Demo, code, stderr, stdout};
use std::process::Command;

#[test]
fn missing_shallow_history_names_fetch_depth_without_changing_revision_verdicts() {
    let source = Demo::new("refusal2333-source");
    source.table("table: dbo.t\ncolumns:\n  id: {type: int}\n");
    assert_eq!(code(&source.run(&["plan", "--no-dev"])), 0);
    source.commit();
    std::fs::write(source.dir.join("second.txt"), "second commit\n").unwrap();
    source.commit();
    let shallow = Demo::new("refusal2333-shallow");
    std::fs::remove_dir_all(&shallow.root).unwrap();
    let cloned = Command::new("git")
        .args(["clone", "--quiet", "--depth", "1"])
        .arg(format!("file://{}", source.root.to_str().unwrap()))
        .arg(&shallow.root)
        .output()
        .unwrap();
    assert!(cloned.status.success(), "{cloned:?}");

    for project in [&source, &shallow] {
        for (args, expected) in [
            (vec!["plan", "--no-dev", "--since", "HEAD~1"], 0),
            (vec!["validate", "--since", "HEAD~1"], 0),
            (vec!["plan", "--no-dev", "--since", "missing"], 1),
            (vec!["validate", "--since", "missing"], 2),
        ] {
            let out = project.run(&args);
            let message = format!("{}{}", stdout(&out), stderr(&out));
            let is_shallow = project.root == shallow.root;
            let expected = if is_shallow && expected == 0 {
                if args[0] == "validate" { 2 } else { 1 }
            } else {
                expected
            };
            assert_eq!(code(&out), expected, "{args:?}: {message}");
            assert_eq!(
                message.contains("fetch-depth: 0"),
                is_shallow,
                "{args:?}: {message}"
            );
            if expected != 0 {
                assert!(
                    message.contains("not a revision this repository has"),
                    "{message}"
                );
            }
        }
        // Trace both successful readers: detecting shallowness must add no
        // Git call or guidance to a revision that already resolves.
        for args in [
            vec!["plan", "--no-dev", "--since", "HEAD"],
            vec!["validate", "--since", "HEAD"],
        ] {
            let trace = project.root.join(format!("{}.trace", args[0]));
            let out = Command::new(BIN)
                .arg("--project")
                .arg(&project.dir)
                .args(&args)
                .env("GIT_TRACE", &trace)
                .output()
                .unwrap();
            assert_eq!(code(&out), 0, "{}{}", stdout(&out), stderr(&out));
            assert!(!stdout(&out).contains("fetch-depth"));
            assert!(!stderr(&out).contains("shallow clone"));
            let trace = std::fs::read_to_string(trace).unwrap();
            assert!(!trace.contains("--is-shallow-repository"), "{trace}");
        }
    }
}
