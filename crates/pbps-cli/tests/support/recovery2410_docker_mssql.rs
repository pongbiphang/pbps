//! A real `pbps plan --dev` on a Docker SQL Server scratch killed with SIGKILL,
//! then the next one (#2410, DEC-2410.3): the container the killed run left
//! running is removed by the next run. Only the containers of the processes the
//! test started are looked at.

use super::*;
use std::process::Stdio;
use std::time::{Duration, Instant};

const LIMIT: Duration = Duration::from_secs(180);

fn containers_of(pid: u32) -> Vec<String> {
    let out = Command::new("docker")
        .args([
            "ps",
            "--all",
            "--no-trunc",
            "--filter",
            "label=pbps.resolver=scratch",
            "--format",
            "{{.Names}} {{.Label \"pbps.resolver.process\"}}",
        ])
        .output()
        .expect("docker");
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|line| line.split_once(' '))
        .filter(|(_, process)| process.starts_with(&format!("{pid}-")))
        .map(|(name, _)| name.to_owned())
        .collect()
}

fn exists(name: &str) -> bool {
    Command::new("docker")
        .args(["container", "inspect", name])
        .output()
        .expect("docker")
        .status
        .success()
}

/// Whether the container's server shows a request waiting in `WAITFOR`.
fn waiting(name: &str) -> bool {
    let env = Command::new("docker")
        .args([
            "container",
            "inspect",
            "--format",
            "{{range .Config.Env}}{{println .}}{{end}}",
            name,
        ])
        .output()
        .expect("docker");
    let env = String::from_utf8_lossy(&env.stdout).into_owned();
    let Some(password) = env
        .lines()
        .find_map(|l| l.strip_prefix("MSSQL_SA_PASSWORD="))
    else {
        return false;
    };
    let out = Command::new("docker")
        .args([
            "exec",
            name,
            "/opt/mssql-tools18/bin/sqlcmd",
            "-C",
            "-S",
            "localhost",
            "-U",
            "sa",
            "-P",
            password,
            "-h",
            "-1",
            "-W",
            "-Q",
            "SET NOCOUNT ON; SELECT COUNT(*) FROM sys.dm_exec_requests WHERE wait_type = N'WAITFOR';",
        ])
        .output()
        .expect("docker");
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .last()
        .and_then(|n| n.trim().parse::<usize>().ok())
        .unwrap_or(0)
        > 0
}

/// Killed inside its externals file, with its keeper connected: the next run
/// removes the container.
pub fn killed_inside_the_externals_file() {
    let image = std::env::var("PBPS_TEST_DEV_IMAGE")
        .expect("PBPS_TEST_DEV_IMAGE must name the rehearsal fixture");
    let d = Demo::new("rec-dk-ms");
    d.table(ONE_COLUMN);
    std::fs::write(
        d.dir.join("pbps.yml"),
        format!(
            "dialect: mssql\nresolvers:\n  scratch: {{kind: docker, image: '{image}', \
             externals: externals.sql}}\ndev: {{resolver: scratch}}\n"
        ),
    )
    .unwrap();
    std::fs::write(d.dir.join("externals.sql"), "SELECT 1;\n").unwrap();
    assert_eq!(code(&d.run(&["plan", "--no-dev"])), 0);
    d.commit();
    d.table(
        "table: dbo.t\ncolumns:\n  id: {type: bigint, nullable: false}\n  \
         extra: {type: int, nullable: true}\n",
    );
    std::fs::write(d.dir.join("externals.sql"), "WAITFOR DELAY '00:01:30';\n").unwrap();
    let mut child = Command::new(BIN)
        .arg("--project")
        .arg(&d.dir)
        .arg("plan")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let pid = child.id();
    let started = Instant::now();
    let until = |what: &str, ready: &dyn Fn() -> bool| {
        while !ready() {
            assert!(started.elapsed() < LIMIT, "gave up waiting for {what}");
            std::thread::sleep(Duration::from_millis(100));
        }
    };
    until("the run's container", &|| !containers_of(pid).is_empty());
    let name = containers_of(pid).remove(0);
    until("the run inside its externals file", &|| waiting(&name));
    child.kill().unwrap();
    child.wait().unwrap();
    assert!(exists(&name), "a killed run leaves its container");
    std::fs::write(d.dir.join("externals.sql"), "SELECT 1;\n").unwrap();
    let next = d.run(&["plan"]);
    let left = exists(&name);
    let _ = Command::new("docker")
        .args(["rm", "--force", "--volumes", &name])
        .output();
    assert_eq!(code(&next), 0, "{}{}", stdout(&next), stderr(&next));
    assert!(!left, "the next run removed it");
    assert!(
        stderr(&next).contains(&name),
        "and said so: {}",
        stderr(&next)
    );
}
