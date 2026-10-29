//! A failed start proves preservation only after the fixture completed its swap.
use super::*;
use pbps_cli::resolver::server::scratch::container::{Held, Owner};
use std::os::unix::fs::PermissionsExt;

const LABEL: &str = "pbps.test-scratch.swap-2505";
const SETUP: &str = "scratch name-swap setup did not complete";
const CASE: &str = "test_scratch::tests::live::a_start_failing_after_its_id_is_recorded_removes_that_container_not_the_one_now_holding_its_name";

/// Even an error after creation can precede the callback and its ID receipt.
/// Find only this fixture's unpredictable label, then remove immutable IDs.
struct Made(String);
impl Made {
    fn label(&self) -> String {
        format!("{LABEL}={}", self.0)
    }
    async fn cleanup(&self) {
        for id in Docker
            .list(&self.label())
            .await
            .expect("list owned swap fixtures")
        {
            let object = Docker
                .inspect(&id)
                .await
                .expect("inspect owned swap fixture")
                .expect("a listed swap fixture exists");
            assert_eq!(object.id, id);
            assert_eq!(object.labels.get(LABEL), Some(&self.0));
            Docker
                .remove(&id)
                .await
                .expect("remove owned swap fixture and volumes");
        }
        assert!(
            Docker
                .list(&self.label())
                .await
                .expect("confirm fixture cleanup")
                .is_empty()
        );
    }
}
impl Drop for Made {
    fn drop(&mut self) {
        // A panic still releases only this fixture's resources. The caller
        // confirms cleanup separately; a failed command is never absence.
        let listing = std::process::Command::new("timeout")
            .args([
                "--kill-after=5s",
                "20s",
                "docker",
                "ps",
                "--all",
                "--no-trunc",
                "--filter",
                &format!("label={}", self.label()),
                "--format",
                "{{.ID}}",
            ])
            .output();
        match listing {
            Ok(output) if output.status.success() => {
                for id in String::from_utf8_lossy(&output.stdout).split_whitespace() {
                    if id.len() != 64 || !id.bytes().all(|b| b.is_ascii_hexdigit()) {
                        eprintln!("unreadable swap fixture ID: {id}");
                        continue;
                    }
                    let removed = std::process::Command::new("timeout")
                        .args([
                            "--kill-after=5s",
                            "20s",
                            "docker",
                            "rm",
                            "--force",
                            "--volumes",
                            id,
                        ])
                        .output();
                    if !removed.is_ok_and(|r| r.status.success()) {
                        eprintln!("swap fixture cleanup was not confirmed for {id}");
                    }
                }
            }
            Ok(_) | Err(_) => {
                eprintln!("swap fixture enumeration was unreadable during panic cleanup")
            }
        }
    }
}
struct Swap {
    original: String,
    replacement: String,
}
struct Swapping {
    token: String,
    moved: String,
    completed: Option<Swap>,
}
impl Owner for Swapping {
    fn token(&self) -> String {
        self.token.clone()
    }
    fn labels(&self) -> Vec<String> {
        vec![
            format!("{RUN_LABEL}={}", self.token),
            format!("{LABEL}={}", self.token),
        ]
    }
    fn recoverable(&self) -> bool {
        true
    }
    async fn before_create(&mut self) -> Result<(), String> {
        Ok(())
    }
    async fn created(&mut self, id: &str) -> Result<(), String> {
        let name = format!("pbps-scratch-{}", self.token);
        docker(&["rename", id, &self.moved]).await?;
        let replacement = docker(&[
            "create",
            "--name",
            &name,
            "--label",
            &format!("{LABEL}={}", self.token),
            &image(),
        ])
        .await?;
        let replacement = replacement.trim();
        let observed = Docker
            .inspect(replacement)
            .await
            .map_err(|e| e.to_string())?
            .ok_or_else(|| "the replacement was not created".to_owned())?;
        if observed.id != replacement || observed.name != name {
            return Err("the replacement's identity or name differs".into());
        }
        self.completed = Some(Swap {
            original: id.to_owned(),
            replacement: observed.id,
        });
        Ok(())
    }
}
pub(super) async fn exercise() {
    let _serial = Dir::new();
    let token = std::env::var("PBPS_SWAP2505_TOKEN").unwrap_or_else(|_| record::new_run_id());
    let made = Made(token.clone());
    let mut owner = Swapping {
        moved: format!("pbps-moved-{token}"),
        token: token.clone(),
        completed: None,
    };
    let started =
        Held::start_owned(Driver::Postgres, &image(), PullPolicy::Never, &mut owner).await;
    let failure = started.as_ref().err().map(ToString::to_string);
    let swapped = owner
        .completed
        .as_ref()
        .unwrap_or_else(|| panic!("{SETUP}: {failure:?}"));
    assert!(started.is_err(), "the replacement has no published port");
    assert_ne!(swapped.original, swapped.replacement);
    assert!(
        Docker
            .inspect(&swapped.original)
            .await
            .expect("inspect the original container")
            .is_none(),
        "the immutable container this run started is removed"
    );
    let replacement = Docker
        .inspect(&swapped.replacement)
        .await
        .expect("inspect the replacement container")
        .expect("the recorded replacement is preserved");
    assert_eq!(replacement.id, swapped.replacement);
    assert_eq!(replacement.name, format!("pbps-scratch-{token}"));
    made.cleanup().await;
}

/// Run the real test in isolated processes: neither PATH nor fault state can
/// affect another fixture. The controls must fail before preservation checks.
pub(super) async fn controls() {
    let d = Dir::new();
    let path = std::env::var_os("PATH").expect("PATH");
    let real = std::env::split_paths(&path)
        .map(|p| p.join("docker"))
        .find(|p| p.is_file())
        .expect("docker executable")
        .canonicalize()
        .unwrap();
    let bin = d.base.join("bin");
    std::fs::create_dir(&bin).unwrap();
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o700)).unwrap();
    let wrapper = bin.join("docker");
    std::fs::write(&wrapper, WRAPPER).unwrap();
    std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o700)).unwrap();
    let wrapped_path =
        std::env::join_paths(std::iter::once(bin).chain(std::env::split_paths(&path))).unwrap();
    for mode in ["pass", "before", "after", "inspect-original"] {
        let made = Made(record::new_run_id());
        let events = d.base.join(format!("{mode}.jsonl"));
        let mut command = tokio::process::Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                CASE,
                "--ignored",
                "--nocapture",
                "--test-threads=1",
            ])
            .env("PATH", &wrapped_path)
            .env("PBPS_SWAP2505_CHILD", mode)
            .env("PBPS_SWAP2505_TOKEN", &made.0)
            .env("PBPS_SWAP2505_REAL_DOCKER", &real)
            .env("PBPS_SWAP2505_EVENTS", &events)
            .kill_on_drop(true);
        let output = tokio::time::timeout(Duration::from_secs(120), command.output())
            .await
            .expect("swap setup control child timed out")
            .expect("run swap setup control");
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        made.cleanup().await;
        assert_eq!(
            output.status.code(),
            Some(if mode == "pass" { 0 } else { 101 }),
            "{mode}: the real fixture must accept only the successful swap: {text}"
        );
        let rows: Vec<serde_json::Value> = std::fs::read_to_string(&events)
            .unwrap()
            .lines()
            .map(|s| serde_json::from_str(s).unwrap())
            .collect();
        let runs: Vec<_> = rows.iter().filter(|v| v["command"] == "run").collect();
        assert_eq!(runs.len(), 1, "{mode}: the control must reach creation");
        if mode == "pass" {
            assert!(runs[0]["real_created"].as_bool().unwrap());
            assert_eq!(rows.iter().filter(|v| v["command"] == "rename").count(), 1);
            assert_eq!(rows.iter().filter(|v| v["command"] == "create").count(), 1);
        } else if mode == "inspect-original" {
            assert!(
                text.contains("inspect the original container"),
                "{mode}: {text}"
            );
            assert!(rows.iter().any(|v| v["command"] == "rename"));
            assert!(rows.iter().any(|v| v["command"] == "create"));
        } else {
            assert!(
                text.contains(SETUP),
                "{mode}: diagnose setup, not preservation: {text}"
            );
            assert!(
                !rows
                    .iter()
                    .any(|v| v["command"] == "rename" || v["command"] == "create")
            );
            assert_eq!(runs[0]["real_created"], mode == "after");
        }
        for row in runs {
            if let Some(id) = row["id"].as_str() {
                assert!(
                    Docker
                        .inspect(id)
                        .await
                        .expect("confirm control's original cleanup")
                        .is_none()
                );
            }
        }
        eprintln!("swap control {mode}: setup boundary and owned cleanup confirmed");
    }
}

const WRAPPER: &str = r#"#!/usr/bin/env python3
import json, os, subprocess, sys
args = sys.argv[1:]
mode = os.environ['PBPS_SWAP2505_CHILD']
log = os.environ['PBPS_SWAP2505_EVENTS']
# This fixture tests identity after creation, not the shared recovery sweep.
# Restrict enumeration even before creation so no unrelated run is touched.
if args[0] == 'ps':
    args += ['--filter', 'label=pbps.test-scratch.swap-2505=' + os.environ['PBPS_SWAP2505_TOKEN']]
def record(row):
    fd = os.open(log, os.O_WRONLY | os.O_APPEND | os.O_CREAT, 0o600)
    with os.fdopen(fd, 'a') as f: f.write(json.dumps(row) + '\n')
if args[0] == 'run' and mode == 'before':
    record(dict(command='run', real_created=False))
    sys.stderr.write('injected failure before creation\n')
    sys.exit(125)
if args[:2] == ['container', 'inspect'] and mode == 'inspect-original':
    with open(log) as f: rows = [json.loads(s) for s in f]
    if any(v.get('command') == 'run' and v.get('id') == args[-1] for v in rows):
        record(dict(command='inspect-original', refused=True))
        sys.stderr.write('injected unreadable original inspection\n')
        sys.exit(125)
p = subprocess.run([os.environ['PBPS_SWAP2505_REAL_DOCKER'], *args], capture_output=True)
row = dict(command=args[0])
if args[0] == 'run':
    row.update(real_created=p.returncode == 0, id=p.stdout.decode().strip() if p.returncode == 0 else None)
record(row)
sys.stdout.buffer.write(p.stdout)
sys.stderr.buffer.write(p.stderr)
if args[0] == 'run' and mode == 'after' and p.returncode == 0:
    sys.stderr.write('injected failure after creation before callback\n')
    sys.exit(125)
sys.exit(p.returncode)
"#;
