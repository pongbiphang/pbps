//! Shared scratch acquisition, exercised through the preview CLI (#587).
use super::*;

#[test]
fn shared_selection_is_lazy_and_rejects_ambiguous_or_unknown_dev_entries() {
    let d = Demo::new("dev587-selection");
    d.table(ONE_COLUMN);
    let configuration = "dialect: mssql\nresolvers:\n  scratch: {kind: server, url_env: PBPS_DEV587_UNSET}\nresolve_with: scratch\n";
    std::fs::write(d.dir.join("pbps.yml"), configuration).unwrap();
    assert_eq!(code(&d.run(&["plan"])), 0);
    for (dev, diagnostic) in [
        ("{resolver: missing}", "missing"),
        ("{resolver: scratch, docker: image}", "do not combine"),
        ("{resolver: scratch}", "PBPS_DEV587_UNSET"),
    ] {
        std::fs::write(
            d.dir.join("pbps.yml"),
            format!("{configuration}dev: {dev}\n"),
        )
        .unwrap();
        let o = d.run(&["plan", "--format", "json"]);
        assert_eq!(code(&o), 1, "{}", stdout(&o));
        assert!(stdout(&o).contains(diagnostic), "{}", stdout(&o));
        assert_eq!(code(&d.run(&["plan", "--no-dev"])), 0);
    }
    d.commit();
    assert_eq!(code(&d.run(&["plan", "--check"])), 0);
    for args in [
        vec!["plan", "--check", "--dev", "docker://unused"],
        vec!["plan", "--env", "unused", "--dev", "docker://unused"],
    ] {
        let o = d.run(&args);
        assert_eq!(code(&o), 1);
        assert!(
            stderr(&o).contains("cannot take --dev") || stderr(&o).contains("separately"),
            "{}",
            stderr(&o)
        );
    }
}

#[cfg(unix)]
#[test]
fn unreadable_shared_credentials_name_the_variable_without_printing_its_value() {
    use std::os::unix::ffi::OsStringExt;
    let d = Demo::new("dev587-secret");
    d.table(ONE_COLUMN);
    std::fs::write(d.dir.join("pbps.yml"), "dialect: mssql\nresolvers:\n  scratch: {kind: server, url_env: DEV587_BAD_TEXT}\ndev: {resolver: scratch}\n").unwrap();
    let secret = std::ffi::OsString::from_vec(b"Dev587Secret\xff".to_vec());
    let o = Command::new(BIN)
        .arg("--project")
        .arg(&d.dir)
        .args(["plan", "--format", "json"])
        .env("DEV587_BAD_TEXT", secret)
        .output()
        .unwrap();
    assert_eq!(code(&o), 1);
    assert!(
        stdout(&o).contains("DEV587_BAD_TEXT") && stdout(&o).contains("not Unicode text"),
        "{}",
        stdout(&o)
    );
    assert!(!stdout(&o).contains("Dev587Secret") && !stderr(&o).contains("Dev587Secret"));
}

fn resources(server: &str) -> Vec<String> {
    tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
        let mut conn = connect_live(server).await.unwrap();
        conn.query("SELECT name AS resource FROM sys.databases WHERE name LIKE 'pbps_scratch_%' UNION ALL SELECT name FROM sys.server_principals WHERE name LIKE 'pbps_run_%' ORDER BY resource;").await.unwrap().iter().map(|row| row.try_get::<&str>("resource").unwrap().unwrap().to_owned()).collect()
    })
}

struct Supplied {
    database: OwnDatabase,
    login: String,
    connection: String,
}
impl Supplied {
    fn new(server: &str, slug: &str) -> Self {
        let database = OwnDatabase::new(server, slug);
        let login = format!("pbps_dev587_{slug}_{}", std::process::id());
        let password = "Pbps!Dev587Supplied";
        on_server(
            server,
            &format!("CREATE LOGIN [{login}] WITH PASSWORD='{password}', CHECK_POLICY=OFF;"),
        );
        on_server(
            database.connection(),
            &format!(
                "ALTER AUTHORIZATION ON DATABASE::[{}] TO [{login}];",
                database.name()
            ),
        );
        let base = server
            .split(';')
            .filter(|part| {
                !matches!(
                    part.split('=')
                        .next()
                        .unwrap_or("")
                        .trim()
                        .to_ascii_lowercase()
                        .as_str(),
                    "user id" | "uid" | "password" | "pwd" | "database" | "initial catalog"
                )
            })
            .collect::<Vec<_>>()
            .join(";");
        let connection = format!(
            "{};User Id={login};Password={password};Database={}",
            base.trim_end_matches(';'),
            database.name()
        );
        Self {
            database,
            login,
            connection,
        }
    }
    fn empty(&self) {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let mut conn = connect_live(self.database.connection()).await.unwrap();
            let inventory = pbps_mssql::resolver::scratch::inventory::read(&mut conn, None)
                .await
                .unwrap();
            assert!(inventory.is_empty(), "{:?}", inventory.names());
        });
    }
}
impl Drop for Supplied {
    fn drop(&mut self) {
        // OwnDatabase drops afterwards; terminate connections before removing the login.
        after_test_on_server(
            &self.database.server,
            &format!(
                "USE master; ALTER DATABASE [{0}] SET SINGLE_USER WITH ROLLBACK IMMEDIATE; DECLARE @sql nvarchar(max)=N'ALTER AUTHORIZATION ON DATABASE::[{0}] TO '+QUOTENAME(SUSER_SNAME()); EXEC(@sql); DROP LOGIN [{1}];",
                self.database.name(),
                self.login
            ),
        );
    }
}

#[test]
#[ignore = "needs live mssql"]
fn shared_entry_stages_externals_before_baseline_and_empties_supplied_scratch() {
    let server = std::env::var("PBPS_TEST_DB").unwrap();
    let scratch = Supplied::new(&server, "externals587");
    let d = Demo::new("dev587-externals");
    std::fs::write(d.dir.join("pbps.yml"), "dialect: mssql\nresolvers:\n  scratch: {kind: server, url_env: DEV587_SCRATCH, externals: externals.sql}\nresolve_with: scratch\ndev: {resolver: scratch}\n").unwrap();
    std::fs::write(
        d.dir.join("externals.sql"),
        "CREATE TABLE dbo.external587(i int);\nGO\n",
    )
    .unwrap();
    d.module(
        "v.yml",
        "view: dbo.preview587\ndefinition: |\n  SELECT i FROM dbo.external587;\n",
    );
    let baseline = d.run(&["plan", "--no-dev"]);
    assert_eq!(
        code(&baseline),
        0,
        "{} {}",
        stdout(&baseline),
        stderr(&baseline)
    );
    d.commit();
    // The baseline itself needs the external table: staging after it would fail.
    for format in ["human", "json"] {
        let o = d.run_with_env(
            &["plan", "--format", format],
            &[("DEV587_SCRATCH", &scratch.connection)],
        );
        assert_eq!(code(&o), 0, "{} {}", stdout(&o), stderr(&o));
        let out = stdout(&o);
        assert!(out.contains("operator-vouched"), "{out}");
        assert!(out.contains("not compared with any target"), "{out}");
        if format == "json" {
            let v: serde_json::Value = serde_json::from_str(&out).unwrap();
            assert!(
                v["findings"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|f| f["id"] == "rehearsal.externals"),
                "{v}"
            );
        }
        scratch.empty();
    }
    std::fs::write(
        d.dir.join("externals.sql"),
        "CREATE TABLE dbo.partial587(i int);\nGO\nSELECT * FROM dbo.absent587;\n",
    )
    .unwrap();
    let refused = d.run_with_env(
        &["plan", "--format", "json"],
        &[("DEV587_SCRATCH", &scratch.connection)],
    );
    assert_eq!(code(&refused), 1, "{}", stdout(&refused));
    assert!(
        stdout(&refused).contains("absent587"),
        "{}",
        stdout(&refused)
    );
    scratch.empty();
}

#[test]
#[ignore = "needs live mssql"]
fn supplied_scratch_refuses_foreign_contents_without_creating_or_cleaning_them() {
    let server = std::env::var("PBPS_TEST_DB").unwrap();
    let scratch = Supplied::new(&server, "dirty587");
    on_server(
        scratch.database.connection(),
        "CREATE TABLE dbo.foreign587(i int); INSERT dbo.foreign587 VALUES (587);",
    );
    let d = Demo::new("dev587-dirty");
    d.table(ONE_COLUMN);
    let refused = d.run(&["plan", "--dev", &scratch.connection, "--format", "json"]);
    assert_eq!(code(&refused), 1, "{}", stdout(&refused));
    assert!(
        stdout(&refused).contains("not empty") && stdout(&refused).contains("foreign587"),
        "{}",
        stdout(&refused)
    );
    on_server(
        scratch.database.connection(),
        "IF (SELECT COUNT(*) FROM dbo.foreign587 WHERE i=587) <> 1 THROW 51000, 'foreign data changed', 1; IF EXISTS(SELECT 1 FROM sys.database_principals WHERE name LIKE 'pbps_sandbox_%') THROW 51000, 'sandbox created before refusal', 1;",
    );
}

#[test]
#[ignore = "needs live mssql"]
fn supplied_scratch_refuses_nonfresh_options_before_setup() {
    let server = std::env::var("PBPS_TEST_DB").unwrap();
    let scratch = Supplied::new(&server, "options587");
    let d = Demo::new("dev587-options");
    d.table(ONE_COLUMN);
    on_server(
        scratch.database.connection(),
        "ALTER DATABASE CURRENT SET AUTO_SHRINK ON;",
    );
    let refused = d.run(&["plan", "--dev", &scratch.connection, "--format", "json"]);
    assert_eq!(code(&refused), 1, "{}", stdout(&refused));
    assert!(
        stdout(&refused).contains("is_auto_shrink_on"),
        "{}",
        stdout(&refused)
    );
    on_server(
        scratch.database.connection(),
        "IF EXISTS(SELECT 1 FROM sys.database_principals WHERE name LIKE 'pbps_sandbox_%') THROW 51000, 'sandbox created before option refusal', 1; ALTER DATABASE CURRENT SET AUTO_SHRINK OFF;",
    );
}

#[test]
#[ignore = "needs live mssql"]
fn inherited_model_objects_refuse_run_owned_rehearsal_and_release_resources() {
    let server = std::env::var("PBPS_TEST_DB").unwrap();
    let d = Demo::new("dev587-model");
    d.table(ONE_COLUMN);
    let before = resources(&server);
    // Read the newborn itself: model's objects are inherited by scratch and reference.
    on_server(
        &server,
        "EXEC(N'USE model; CREATE TABLE dbo.inherited587(i int);');",
    );
    let inherited = d.run(&["plan", "--dev", &server, "--format", "json"]);
    on_server(&server, "EXEC(N'USE model; DROP TABLE dbo.inherited587;');");
    assert_eq!(code(&inherited), 1, "{}", stdout(&inherited));
    assert!(
        stdout(&inherited).contains("inherited587"),
        "{}",
        stdout(&inherited)
    );
    assert_eq!(resources(&server), before);
}

#[cfg(unix)]
#[test]
#[ignore = "needs live mssql"]
fn rehearsal_signals_empty_supplied_scratch_and_remove_owned_databases_only() {
    use std::time::{Duration, Instant};
    let server = std::env::var("PBPS_TEST_DB").unwrap();
    let scratch = Supplied::new(&server, "cancel587");
    let sentinel = OwnDatabase::new(&server, "sentinel587");
    on_server(
        sentinel.connection(),
        "CREATE TABLE dbo.keep587(i int); INSERT dbo.keep587 VALUES(587);",
    );
    let d = Demo::new("dev587-cancel");
    d.table(ONE_COLUMN);
    std::fs::write(d.dir.join("pbps.yml"), "dialect: mssql\nresolvers:\n  scratch: {kind: server, url_env: DEV587_SCRATCH, externals: externals.sql}\ndev: {resolver: scratch}\n").unwrap();
    // A committed external object proves that acquisition finished and the workload is running.
    std::fs::write(
        d.dir.join("externals.sql"),
        "CREATE TABLE dbo.running587(i int);\nGO\nWAITFOR DELAY '00:01:00';\n",
    )
    .unwrap();
    let before = resources(&server);
    for (signal, connection, supplied) in [
        ("INT", scratch.connection.as_str(), true),
        ("TERM", scratch.connection.as_str(), true),
        ("INT", server.as_str(), false),
        ("TERM", server.as_str(), false),
    ] {
        let mut child = Command::new(BIN)
            .arg("--project")
            .arg(&d.dir)
            .args(["plan", "--format", "json"])
            .env("DEV587_SCRATCH", connection)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let started = Instant::now();
        loop {
            let probe = if supplied {
                "IF OBJECT_ID(N'dbo.running587') IS NULL THROW 51000, 'waiting', 1;".to_owned()
            } else {
                "DECLARE @found int=0; DECLARE @sql nvarchar(max)=N''; SELECT @sql+=N'IF OBJECT_ID(N'''+name+N'.dbo.running587'') IS NOT NULL SET @found=1;' FROM sys.databases WHERE name LIKE 'pbps_scratch_%'; EXEC sp_executesql @sql, N'@found int OUTPUT', @found OUTPUT; IF @found=0 THROW 51000, 'waiting', 1;".into()
            };
            if try_on_server(
                if supplied {
                    scratch.database.connection()
                } else {
                    &server
                },
                &probe,
            )
            .is_ok()
            {
                break;
            }
            assert!(
                child.try_wait().unwrap().is_none(),
                "rehearsal ended before cancellation"
            );
            if started.elapsed() > Duration::from_secs(40) {
                let _ = child.kill();
                panic!("rehearsal never reached the workload");
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        assert!(
            Command::new("kill")
                .args([&format!("-{signal}"), &child.id().to_string()])
                .status()
                .unwrap()
                .success()
        );
        let o = child.wait_with_output().unwrap();
        assert_eq!(code(&o), 1, "{} {}", stdout(&o), stderr(&o));
        assert!(stdout(&o).contains("cancelled"), "{}", stdout(&o));
        scratch.empty();
        on_server(
            sentinel.connection(),
            "IF (SELECT COUNT(*) FROM dbo.keep587 WHERE i=587)<>1 THROW 51000, 'foreign database changed', 1;",
        );
        assert_eq!(
            resources(&server),
            before,
            "cleanup changed resources beyond the run"
        );
    }
}

#[cfg(unix)]
fn runtime_fixture(d: &Demo) -> std::ffi::OsString {
    use std::os::unix::fs::PermissionsExt;
    let bin = d.dir.join("bin");
    std::fs::create_dir(&bin).unwrap();
    let docker = bin.join("docker");
    std::fs::write(
        &docker,
        r#"#!/bin/sh
case "$1" in
  image) printf 'image-id\n';;
  run)
    shift
    while [ "$#" -gt 0 ]; do
      case "$1" in
        --name) printf '%s' "$2" > "$PBPS_DEV_ID";;
        --publish) printf 'publish %s\n' "$2" >> "$PBPS_DEV_LOG";;
        MSSQL_SA_PASSWORD=*) printf '%s' "${1#MSSQL_SA_PASSWORD=}" > "$PBPS_DEV_SECRET";;
      esac
      shift
    done
    printf 'runtime-id\n';;
  port) printf '%s\n' "$PBPS_DEV_MAPPING"; exit "$PBPS_DEV_PORT_STATUS";;
  inspect)
    if [ ! -e "$PBPS_DEV_ONCE" ]; then
      : > "$PBPS_DEV_ONCE"
      case "$PBPS_DEV_FIRST" in
        malformed) printf '{}\n'; exit 0;;
        denied) printf 'permission denied\n' >&2; exit 1;;
        hung) exec sleep 30;;
      esac
    fi
    if [ "$PBPS_DEV_FIRST" = missing ]; then
      printf 'Error: no such object: "%s"\n' "$(cat "$PBPS_DEV_ID")" >&2; exit 125
    fi
    printf '{"Running":false,"ExitCode":137,"OOMKilled":false,"Error":""}\n';;
  logs) printf 'startup failure; password='; cat "$PBPS_DEV_SECRET";;
  container)
    if [ "$2" = ls ]; then exit 0; fi
    if [ -e "$PBPS_DEV_REMOVED" ]; then
      printf 'Error: no such container\n' >&2; exit 1
    fi
    case "$4" in
      '{{.Id}}') printf 'runtime-id\n';;
      '{{range .Mounts}}{{.Name}} {{end}}') exit 0;;
      '{{.State.Status}}') printf 'exited\n';;
      *) exit 99;;
    esac;;
  kill) printf '%s\n' "$*" >> "$PBPS_DEV_LOG";;
  rm)
    printf '%s\n' "$*" >> "$PBPS_DEV_LOG"
    : > "$PBPS_DEV_REMOVED";;
  *) exit 99;;
esac
"#,
    )
    .unwrap();
    std::fs::set_permissions(&docker, std::fs::Permissions::from_mode(0o755)).unwrap();
    // A successful exec proves inherited write descriptors have closed. The
    // image branch has no fixture effects, so later product invocations need
    // no retry of stringified failures from inside the CLI (#2554).
    let ready = stand_in::retry_exec(|| Command::new(&docker).arg("image").output()).unwrap();
    assert!(ready.status.success(), "{ready:?}");
    let mut paths = vec![bin];
    paths.extend(std::env::split_paths(&std::env::var_os("PATH").unwrap()));
    std::env::join_paths(paths).unwrap()
}

#[cfg(unix)]
pub(super) fn runtime_refusals(mapping_case: bool) {
    let d = Demo::new(if mapping_case {
        "dev587-mapping"
    } else {
        "dev587-startup"
    });
    d.table(ONE_COLUMN);
    let path = runtime_fixture(&d);
    let log = d.dir.join("docker.log");
    let secret = d.dir.join("secret");
    let id = d.dir.join("id");
    let once = d.dir.join("once");
    let removed = d.dir.join("removed");
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let mapping = format!("127.0.0.1:{}", listener.local_addr().unwrap().port());
    drop(listener);
    let cases = if mapping_case {
        vec![
            ("0.0.0.0:49154", "0", "", "loopback"),
            ("lookup unavailable", "17", "", "published port"),
            ("127.0.0.1:49154", "17", "", "published port"),
        ]
    } else {
        ["stopped", "malformed", "denied", "hung", "missing"]
            .into_iter()
            .map(|first| {
                (
                    mapping.as_str(),
                    "0",
                    first,
                    if first == "missing" {
                        "no longer exists"
                    } else {
                        "exit status 137, OOM killed: false"
                    },
                )
            })
            .collect()
    };
    for (mapping, status, first, diagnostic) in cases {
        std::fs::write(&log, "").unwrap();
        let _ = std::fs::remove_file(&once);
        let _ = std::fs::remove_file(&removed);
        let started = std::time::Instant::now();
        let mut command = Command::new(BIN);
        command
            .arg("--project")
            .arg(&d.dir)
            .args(["plan", "--dev", "docker://stand-in", "--format", "json"])
            .env("PATH", &path)
            .env("PBPS_DEV_LOG", &log)
            .env("PBPS_DEV_SECRET", &secret)
            .env("PBPS_DEV_ID", &id)
            .env("PBPS_DEV_ONCE", &once)
            .env("PBPS_DEV_REMOVED", &removed)
            .env("PBPS_DEV_FIRST", first)
            .env("PBPS_DEV_MAPPING", mapping)
            .env("PBPS_DEV_PORT_STATUS", status);
        let o = command.output().unwrap();
        assert_eq!(code(&o), 1, "{} {}", stdout(&o), stderr(&o));
        let v: serde_json::Value = serde_json::from_str(&stdout(&o)).unwrap();
        assert_eq!(v["result"], "unanswerable", "{v}");
        assert!(
            v["findings"]
                .as_array()
                .unwrap()
                .iter()
                .any(|f| f["id"] == "rehearsal.unavailable" && f.to_string().contains(diagnostic)),
            "{v}"
        );
        let name = std::fs::read_to_string(&id).unwrap();
        assert!(name.starts_with("pbps-scratch-"));
        // Removed by the id `docker run` printed, not by the reusable name.
        assert_eq!(
            std::fs::read_to_string(&log).unwrap(),
            "publish 127.0.0.1::1433\nkill runtime-id\nrm --force --volumes runtime-id\n"
        );
        if !mapping_case {
            assert!(
                stdout(&o).contains("startup failure; password=[redacted]"),
                "{v}"
            );
            let password = std::fs::read_to_string(&secret).unwrap();
            assert!(!password.is_empty());
            assert!(!stdout(&o).contains(&password) && !stderr(&o).contains(&password));
        }
        assert!(started.elapsed() < std::time::Duration::from_secs(45));
    }
}

#[cfg(unix)]
#[test]
#[ignore = "needs docker and PBPS_TEST_DEV_IMAGE"]
fn docker_signals_remove_the_rehearsal_container_and_its_volumes() {
    use std::time::{Duration, Instant};
    let image = std::env::var("PBPS_TEST_DEV_IMAGE")
        .expect("PBPS_TEST_DEV_IMAGE must name the rehearsal fixture");
    let d = Demo::new("dev587-docker-cancel");
    d.table(ONE_COLUMN);
    std::fs::write(d.dir.join("pbps.yml"), format!("dialect: mssql\nresolvers:\n  scratch: {{kind: docker, image: '{image}', externals: externals.sql}}\ndev: {{resolver: scratch}}\n")).unwrap();
    std::fs::write(
        d.dir.join("externals.sql"),
        "CREATE TABLE dbo.running587(i int);\nGO\nWAITFOR DELAY '00:02:00';\n",
    )
    .unwrap();
    for signal in ["INT", "TERM"] {
        let mut child = Command::new(BIN)
            .arg("--project")
            .arg(&d.dir)
            .args(["plan", "--format", "json"])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let started = Instant::now();
        let (id, volumes) = loop {
            let listed = Command::new("docker")
                .args([
                    "ps",
                    "--filter",
                    "label=pbps.resolver=scratch",
                    "--format",
                    "{{.ID}} {{.Label \"pbps.resolver.process\"}}",
                ])
                .output()
                .unwrap();
            assert!(listed.status.success(), "{}", stderr(&listed));
            let owned = stdout(&listed).lines().find_map(|line| {
                let (id, label) = line.split_once(' ')?;
                label
                    .starts_with(&format!("{}-", child.id()))
                    .then(|| id.to_owned())
            });
            if let Some(id) = owned {
                let env = Command::new("docker")
                    .args([
                        "inspect",
                        "--format",
                        "{{range .Config.Env}}{{println .}}{{end}}",
                        &id,
                    ])
                    .output()
                    .unwrap();
                let password = stdout(&env)
                    .lines()
                    .find_map(|line| line.strip_prefix("MSSQL_SA_PASSWORD=").map(str::to_owned))
                    .unwrap();
                let port = Command::new("docker")
                    .args(["port", &id, "1433/tcp"])
                    .output()
                    .unwrap();
                let port = stdout(&port)
                    .trim()
                    .strip_prefix("127.0.0.1:")
                    .map(str::to_owned);
                if let Some(port) = port {
                    let connection = format!(
                        "Server=127.0.0.1,{port};User Id=sa;Password={password};TrustServerCertificate=true"
                    );
                    let probe = "DECLARE @found int=0; DECLARE @sql nvarchar(max)=N''; SELECT @sql+=N'IF OBJECT_ID(N'''+name+N'.dbo.running587'') IS NOT NULL SET @found=1;' FROM sys.databases WHERE name LIKE 'pbps_scratch_%'; EXEC sp_executesql @sql, N'@found int OUTPUT', @found OUTPUT; IF @found=0 THROW 51000, 'waiting', 1;";
                    if try_on_server(&connection, probe).is_ok() {
                        let mounts = Command::new("docker")
                            .args([
                                "inspect",
                                "--format",
                                "{{range .Mounts}}{{.Name}} {{end}}",
                                &id,
                            ])
                            .output()
                            .unwrap();
                        assert!(mounts.status.success());
                        break (id, stdout(&mounts));
                    }
                }
            }
            if child.try_wait().unwrap().is_some() {
                let o = child.wait_with_output().unwrap();
                panic!(
                    "rehearsal ended before cancellation: {} {}",
                    stdout(&o),
                    stderr(&o)
                );
            }
            if started.elapsed() > Duration::from_secs(180) {
                let _ = child.kill();
                panic!("container rehearsal never reached the workload");
            }
            std::thread::sleep(Duration::from_millis(200));
        };
        assert!(
            Command::new("kill")
                .args([&format!("-{signal}"), &child.id().to_string()])
                .status()
                .unwrap()
                .success()
        );
        let o = child.wait_with_output().unwrap();
        assert_eq!(code(&o), 1, "{} {}", stdout(&o), stderr(&o));
        assert!(stdout(&o).contains("cancelled"), "{}", stdout(&o));
        let gone = Command::new("docker")
            .args(["container", "inspect", &id])
            .output()
            .unwrap();
        assert!(
            !gone.status.success() && stderr(&gone).to_ascii_lowercase().contains("no such"),
            "{}",
            stderr(&gone)
        );
        for volume in volumes.split_whitespace() {
            let gone = Command::new("docker")
                .args(["volume", "inspect", volume])
                .output()
                .unwrap();
            assert!(
                !gone.status.success() && stderr(&gone).to_ascii_lowercase().contains("no such"),
                "{}",
                stderr(&gone)
            );
        }
    }
}
