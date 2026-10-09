//! Real process/session loss uses a server lock as the receipt, never elapsed time.

use std::fs::File;
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use pbps_db::{Conn, Driver};

const LIMIT: Duration = Duration::from_secs(30);

struct Running {
    child: Child,
    stdout: std::path::PathBuf,
    stderr: std::path::PathBuf,
    #[cfg(target_os = "linux")]
    pipe: Option<CheckpointOutput>,
}

impl Running {
    fn start(d: &super::Demo, args: &[&str]) -> Self {
        let stdout = d.dir.join("interrupted.stdout");
        let stderr = d.dir.join("interrupted.stderr");
        Self::start_with_output(
            d,
            args,
            Stdio::from(File::create(&stdout).unwrap()),
            stdout,
            stderr,
        )
    }

    fn start_with_output(
        d: &super::Demo,
        args: &[&str],
        output: Stdio,
        stdout: std::path::PathBuf,
        stderr: std::path::PathBuf,
    ) -> Self {
        let child = Command::new(super::BIN)
            .arg("--project")
            .arg(&d.dir)
            .args(args)
            .stdout(output)
            .stderr(Stdio::from(File::create(&stderr).unwrap()))
            .spawn()
            .unwrap();
        Self {
            child,
            stdout,
            stderr,
            #[cfg(target_os = "linux")]
            pipe: None,
        }
    }

    fn finish(&mut self) -> Output {
        let start = Instant::now();
        let status = loop {
            #[cfg(target_os = "linux")]
            if let Some(pipe) = &mut self.pipe {
                pipe.drain();
            }
            if let Some(status) = self.child.try_wait().unwrap() {
                break status;
            }
            assert!(
                start.elapsed() < LIMIT,
                "apply did not exit after disruption/release"
            );
            std::thread::sleep(Duration::from_millis(10));
        };
        #[cfg(target_os = "linux")]
        if let Some(pipe) = &mut self.pipe {
            pipe.drain();
        }
        for path in [&self.stdout, &self.stderr] {
            assert!(
                std::fs::metadata(path).unwrap().len() < 1024 * 1024,
                "unbounded child output"
            );
        }
        Output {
            status,
            stdout: std::fs::read(&self.stdout).unwrap(),
            stderr: std::fs::read(&self.stderr).unwrap(),
        }
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Session {
    id: i64,
    born: i64,
}

struct Engine {
    driver: Driver,
    connection: String,
    observer: std::cell::RefCell<Conn>,
    rt: tokio::runtime::Runtime,
}

impl Engine {
    fn new(driver: Driver, connection: &str) -> Self {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let observer = rt.block_on(async {
            tokio::time::timeout(LIMIT, Conn::connect(driver, connection))
                .await
                .unwrap()
                .unwrap()
        });
        Self {
            driver,
            connection: connection.to_owned(),
            observer: std::cell::RefCell::new(observer),
            rt,
        }
    }

    fn pg(&self) -> bool {
        matches!(self.driver, Driver::Postgres)
    }

    fn connect(&self) -> Conn {
        self.rt.block_on(async {
            tokio::time::timeout(LIMIT, Conn::connect(self.driver, &self.connection))
                .await
                .expect("connection deadline")
                .unwrap()
        })
    }

    fn execute(&self, c: &mut Conn, sql: &str) {
        self.rt.block_on(async {
            tokio::time::timeout(LIMIT, c.execute(sql))
                .await
                .expect("statement deadline")
                .unwrap()
        });
    }

    fn number(&self, sql: &str) -> i64 {
        self.number_on(&mut self.observer.borrow_mut(), sql)
    }

    fn number_on(&self, c: &mut Conn, sql: &str) -> i64 {
        self.rt.block_on(async {
            tokio::time::timeout(LIMIT, c.query(sql))
                .await
                .expect("observation deadline")
                .unwrap()[0]
                .try_get_at::<i64>(0)
                .unwrap()
                .unwrap()
        })
    }

    fn ledger(&self) -> &'static str {
        if self.pg() { "public" } else { "dbo" }
    }

    fn gate(&self) -> (Conn, i64) {
        let mut c = self.connect();
        let id = self.number_on(
            &mut c,
            if self.pg() {
                "SELECT pg_backend_pid()::bigint"
            } else {
                "SELECT CAST(@@SPID AS bigint)"
            },
        );
        self.execute(&mut c, if self.pg() {
            "BEGIN; LOCK TABLE public.__pbps_state IN SHARE MODE"
        } else {
            "BEGIN TRANSACTION; SELECT COUNT_BIG(*) FROM dbo.__pbps_state WITH (TABLOCK, HOLDLOCK)"
        });
        (c, id)
    }

    fn await_request(&self, blocker: i64, child: &mut Running, insert: bool) -> Session {
        let operation = if insert {
            "INSERT INTO"
        } else if self.pg() {
            "DELETE FROM ONLY"
        } else {
            "DELETE FROM"
        };
        let table = if insert {
            "__pbps_state"
        } else {
            "__pbps_lock"
        };
        let sql = if self.pg() {
            format!(
                "SELECT COALESCE(max(pid),0)::bigint FROM pg_stat_activity WHERE datname=current_database() AND {blocker}=ANY(pg_blocking_pids(pid)) AND query LIKE '{operation} public.{table}%' AND xact_start IS NOT NULL"
            )
        } else {
            format!(
                "SELECT CAST(COALESCE(MAX(r.session_id),0) AS bigint) FROM sys.dm_exec_requests r CROSS APPLY sys.dm_exec_sql_text(r.sql_handle) t WHERE r.database_id=DB_ID() AND r.blocking_session_id={blocker} AND t.text LIKE '%{operation} dbo.{table}%' AND r.open_transaction_count>0"
            )
        };
        let start = Instant::now();
        loop {
            assert!(
                child.child.try_wait().unwrap().is_none(),
                "CLI exited before the record gate: {} {}",
                std::fs::read_to_string(&child.stdout).unwrap(),
                std::fs::read_to_string(&child.stderr).unwrap()
            );
            let id = self.number(&sql);
            if id != 0 {
                let (from, key, born) = self.session_columns();
                let born = self.number(&format!("SELECT {born} FROM {from} WHERE {key}={id}"));
                return Session { id, born };
            }
            assert!(
                start.elapsed() < LIMIT,
                "no owned {operation} {table} blocked by session {blocker}"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn session_columns(&self) -> (&'static str, &'static str, &'static str) {
        if self.pg() {
            (
                "pg_stat_activity",
                "pid",
                "(extract(epoch from backend_start)*1000000)::bigint",
            )
        } else {
            (
                "sys.dm_exec_sessions",
                "session_id",
                "DATEDIFF_BIG(microsecond, '20000101', login_time)",
            )
        }
    }

    fn session_present(&self, session: Session) -> bool {
        let (from, key, born) = self.session_columns();
        self.number(&format!(
            "SELECT {}(*) FROM {from} WHERE {key}={} AND {born}={}",
            self.count(),
            session.id,
            session.born
        )) == 1
    }

    fn session_gone(&self, session: Session) {
        let start = Instant::now();
        while self.session_present(session) {
            assert!(
                start.elapsed() < LIMIT,
                "interrupted session {session:?} still present"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn terminate(&self, session: Session) {
        // Never terminate a recycled PID/SPID. The receipt owns its server
        // creation identity as well as its numeric id and isolated database.
        assert!(
            self.session_present(session),
            "owned session already disappeared"
        );
        let (from, key, born) = self.session_columns();
        let predicate = format!("{key}={} AND {born}={}", session.id, session.born);
        if self.pg() {
            assert_eq!(self.number(&format!("SELECT CASE WHEN pg_terminate_backend(pid) THEN 1 ELSE 0 END::bigint FROM {from} WHERE {predicate} AND datname=current_database()")), 1);
        } else {
            self.execute(&mut self.observer.borrow_mut(), &format!("IF EXISTS (SELECT 1 FROM {from} WHERE {predicate}) KILL {}; ELSE THROW 50000, 'owned session disappeared', 1", session.id));
        }
    }

    fn latest(&self) -> pbps_model::StateSnapshot {
        let mut c = self.connect();
        self.rt.block_on(async {
            tokio::time::timeout(LIMIT, async {
                if self.pg() {
                    pbps_pg::state::latest(&mut c)
                        .await
                        .unwrap()
                        .unwrap()
                        .snapshot
                } else {
                    pbps_mssql::state::latest(&mut c)
                        .await
                        .unwrap()
                        .unwrap()
                        .snapshot
                }
            })
            .await
            .expect("snapshot deadline")
        })
    }
}

#[derive(Clone, Copy, Debug)]
pub(super) enum Loss {
    Release,
    Process,
    Session,
}

#[derive(Clone, Copy, Debug)]
pub(super) enum Case {
    Transactional,
    StagedDdl,
    StagedRow,
    Committed,
    #[cfg(target_os = "linux")]
    Checkpoint,
}

pub(super) fn run(connection: &str, driver: Driver, loss: Loss, case: Case) {
    let e = Engine::new(driver, connection);
    e.execute(&mut e.connect(), "CREATE SCHEMA app");
    let d = super::Demo::new(&format!("abrupt-{case:?}-{loss:?}"));
    let table = "table: app.t\ncolumns:\n  id: {type: bigint, nullable: false}\n  label: {type: varchar(20), nullable: false}\nprimary_key: [id]\ndata:\n  mode: exact\n  rows:\n    1: {label: before}\n    2: {label: sentinel}\n";
    std::fs::write(d.dir.join("schema/app.t.yml"), table).unwrap();
    succeeds(d.run(&["plan"]));
    d.commit();
    succeeds(d.run(&["bootstrap", "--db", connection]));
    let baseline = e.latest();
    #[cfg(target_os = "linux")]
    let checkpoint = matches!(case, Case::Checkpoint);
    #[cfg(not(target_os = "linux"))]
    let checkpoint = false;
    let staged = matches!(case, Case::StagedDdl | Case::StagedRow) || checkpoint;
    let ddl = matches!(case, Case::StagedDdl) || checkpoint;
    let changed = if ddl {
        e.execute(&mut e.connect(), "CREATE SCHEMA moved");
        table.replace("table: app.t", "table: moved.u\nrenamed_from: app.t")
    } else {
        table.replace("label: before", "label: after")
    };
    std::fs::write(d.dir.join("schema/app.t.yml"), changed).unwrap();
    succeeds(d.run(&["plan"]));
    d.commit();
    let plan = d.dir.join("recovery.json");
    let mut planning = vec!["plan", "--db", connection, "--out", plan.to_str().unwrap()];
    if staged {
        planning.push("--staged");
    }
    succeeds(d.run(&planning));
    let checksum = super::plan_checksum(&plan);
    let mut args = vec![
        "apply",
        "--db",
        connection,
        "--plan",
        plan.to_str().unwrap(),
        "--checksum",
        &checksum,
        "--allow",
        if ddl { "rename" } else { "data-update" },
    ];
    if staged {
        args.push("--staged");
    }
    let (mut gate, blocker) = e.gate();
    #[cfg(target_os = "linux")]
    let mut child = if checkpoint {
        Running::checkpoint(&d, &args)
    } else {
        Running::start(&d, &args)
    };
    #[cfg(not(target_os = "linux"))]
    let mut child = Running::start(&d, &args);
    let session = e.await_request(blocker, &mut child, true);
    assert!(
        !e.session_present(Session {
            born: session.born + 1,
            ..session
        }),
        "wrong session generation must not match"
    );
    eprintln!("record gate reached: {case:?}/{loss:?}, session {session:?}, blocker {blocker}");
    // A staged step has committed by this receipt, while its first checkpoint
    // is still blocked. Independent reads distinguish it from ordinary rollback.
    if staged {
        assert_eq!(e.latest(), baseline);
        let live_table = if ddl { "moved.t" } else { "app.t" };
        e.rows(live_table, if ddl { "before" } else { "after" });
    }
    #[cfg(target_os = "linux")]
    if checkpoint {
        // Fill stdout while the first checkpoint INSERT is held. The next
        // println is after its durable commit and before the next step. Pipe
        // backpressure holds that write without changing SQL or parsing TLS.
        child.pipe.as_mut().unwrap().arm();
        e.execute(&mut gate, "ROLLBACK");
        child.await_pipe_write();
        let start = Instant::now();
        let recorded = loop {
            assert!(child.child.try_wait().unwrap().is_none());
            let recorded = e.latest();
            if recorded.staged.is_some() {
                break recorded;
            }
            assert_eq!(
                recorded, baseline,
                "CLI advanced past the checkpoint output gate"
            );
            assert!(
                start.elapsed() < LIMIT,
                "first checkpoint did not become durable"
            );
            std::thread::sleep(Duration::from_millis(10));
        };
        let progress = recorded.staged.as_ref().unwrap();
        assert_eq!((progress.completed, progress.total), (1, 2));
        assert_eq!(recorded.plan_checksum.as_deref(), Some(checksum.as_str()));
        assert!(
            recorded
                .ids
                .tables
                .values()
                .any(|t| t.to_string() == "moved.t")
        );
        e.rows("moved.t", "before");
        assert_eq!(e.entries("apply"), 0);
        match loss {
            Loss::Release => (),
            Loss::Process => {
                child.child.kill().unwrap();
                child.child.wait().unwrap();
            }
            Loss::Session => {
                e.terminate(session);
                e.session_gone(session);
            }
        }
        child.pipe.as_mut().unwrap().release();
        let out = child.finish();
        e.session_gone(session);
        if matches!(loss, Loss::Release) {
            succeeds(out);
        } else {
            assert!(!out.status.success());
            assert_eq!(e.latest(), recorded);
            assert_eq!(e.entries("staged"), 1);
            assert_eq!(e.entries("apply"), 0);
            succeeds(d.run(&["unlock", "--db", connection]));
            let mut resume = args.clone();
            resume.push("--resume");
            let mut wrong = resume.clone();
            let checksum_at = wrong.iter().position(|a| *a == "--checksum").unwrap() + 1;
            wrong[checksum_at] = "0000000000000000000000000000000000000000000000000000000000000000";
            let refused = d.run(&wrong);
            assert!(!refused.status.success());
            assert!(String::from_utf8_lossy(&refused.stderr).contains("checksum"));
            assert_eq!(e.latest(), recorded);
            // A real checkpoint permits only its recorded intermediate state.
            e.execute(&mut e.connect(), "ALTER TABLE moved.t ADD rogue bigint");
            let refused = d.run(&resume);
            assert!(!refused.status.success());
            assert!(
                String::from_utf8_lossy(&refused.stderr).contains("has moved since the checkpoint")
            );
            let refused_state = e.latest();
            assert_eq!(refused_state.kind, pbps_model::StateKind::Failed);
            assert_eq!(refused_state.schema, recorded.schema);
            assert_eq!(refused_state.ids, recorded.ids);
            assert_eq!(refused_state.staged, recorded.staged);
            assert_eq!(refused_state.plan_checksum, recorded.plan_checksum);
            assert_eq!(e.entries("apply"), 0);
            assert_eq!(e.entries("staged"), 1);
            e.execute(&mut e.connect(), "ALTER TABLE moved.t DROP COLUMN rogue");
            succeeds(d.run(&resume));
        }
        assert_eq!(e.entries("apply"), 1);
        e.rows("moved.u", "before");
        succeeds(d.run(&["verify", "--db", connection]));
        return;
    }
    if matches!(case, Case::Committed) {
        // Acquire this second gate only after the application's lock row exists.
        // It permits the transaction to commit, but holds its later unlock DELETE.
        let mut unlock_gate = e.connect();
        let unlock_blocker = e.number_on(
            &mut unlock_gate,
            if e.pg() {
                "SELECT pg_backend_pid()::bigint"
            } else {
                "SELECT CAST(@@SPID AS bigint)"
            },
        );
        e.execute(&mut unlock_gate, if e.pg() {
            "BEGIN; SELECT * FROM public.__pbps_lock WHERE id=1 FOR SHARE"
        } else {
            "BEGIN TRANSACTION; SELECT * FROM dbo.__pbps_lock WITH (ROWLOCK, HOLDLOCK) WHERE id=1"
        });
        e.execute(&mut gate, "ROLLBACK");
        assert_eq!(e.await_request(unlock_blocker, &mut child, false), session);
        assert_eq!(e.entries("apply"), 1);
        e.rows("app.t", "after");
        assert!(
            !std::fs::read_to_string(&child.stdout)
                .unwrap()
                .contains("Applied ")
        );
        match loss {
            Loss::Release => (),
            Loss::Process => {
                child.child.kill().unwrap();
                child.child.wait().unwrap();
            }
            Loss::Session => {
                e.terminate(session);
                e.session_gone(session);
            }
        }
        e.execute(&mut unlock_gate, "ROLLBACK");
        let out = child.finish();
        e.session_gone(session);
        if matches!(loss, Loss::Release) {
            succeeds(out);
        } else {
            assert!(!out.status.success());
            succeeds(d.run(&["unlock", "--db", connection]));
        }
        let refused = d.run(&args);
        assert!(!refused.status.success());
        assert!(
            String::from_utf8_lossy(&refused.stderr)
                .contains("is no longer the database this plan was computed against")
        );
        assert_eq!(e.entries("apply"), 1);
        e.rows("app.t", "after");
        succeeds(d.run(&["verify", "--db", connection]));
        return;
    }
    match loss {
        Loss::Release => (),
        Loss::Process => {
            child.child.kill().unwrap();
            child.child.wait().unwrap();
        }
        Loss::Session => {
            e.terminate(session);
            e.session_gone(session);
        }
    }
    e.execute(&mut gate, "ROLLBACK");
    let out = child.finish();
    e.session_gone(session);
    if matches!(loss, Loss::Release) {
        succeeds(out);
    } else {
        assert!(!out.status.success());
        let recovered = e.latest();
        assert_eq!(e.entries("apply"), 0);
        if !staged || matches!(loss, Loss::Session) {
            assert_eq!(recovered, baseline);
            assert_eq!(e.entries("staged"), 0);
        }
        // Engine-session disappearance is established before public stale-lock
        // recovery. A process kill need not append its own failed attempt.
        assert_eq!(
            e.number(&format!(
                "SELECT {}(*) FROM {}.__pbps_lock",
                e.count(),
                e.ledger()
            )),
            1
        );
        succeeds(d.run(&["unlock", "--db", connection]));
        if staged {
            let mut resume = args.clone();
            resume.push("--resume");
            if let Some(progress) = recovered.staged.as_ref() {
                // Process death cannot cancel a command already executing at the
                // server. Its checkpoint may commit after the socket disappears.
                assert!(matches!(loss, Loss::Process));
                assert_eq!(progress.completed, 1);
                assert_eq!(progress.total, if ddl { 2 } else { 1 });
                assert_eq!(recovered.plan_checksum.as_deref(), Some(checksum.as_str()));
                assert_eq!(e.entries("staged"), 1);
                succeeds(d.run(&resume));
                e.rows(
                    if ddl { "moved.u" } else { "app.t" },
                    if ddl { "before" } else { "after" },
                );
                assert_eq!(e.entries("apply"), 1);
                succeeds(d.run(&["verify", "--db", connection]));
                return;
            }
            assert_eq!(recovered, baseline);
            let refused = d.run(&resume);
            assert!(!refused.status.success());
            assert!(
                String::from_utf8_lossy(&refused.stderr)
                    .contains("has no staged apply in progress")
            );
            let refused = d.run(&args);
            assert!(!refused.status.success());
            assert!(
                String::from_utf8_lossy(&refused.stderr)
                    .contains("is no longer the database this plan was computed against")
            );
            e.rows(
                if ddl { "moved.t" } else { "app.t" },
                if ddl { "before" } else { "after" },
            );
            assert_eq!(e.entries("apply"), 0);
            return;
        }
        e.rows("app.t", "before");
        succeeds(d.run(&["verify", "--db", connection]));
        succeeds(d.run(&args));
    }
    e.rows(
        if ddl { "moved.u" } else { "app.t" },
        if ddl { "before" } else { "after" },
    );
    assert_eq!(e.entries("apply"), 1);
    assert_eq!(
        e.number(&format!(
            "SELECT {}(*) FROM {}.__pbps_lock",
            e.count(),
            e.ledger()
        )),
        0
    );
    succeeds(d.run(&["verify", "--db", connection]));
}

impl Engine {
    fn count(&self) -> &'static str {
        if self.pg() { "COUNT" } else { "COUNT_BIG" }
    }
    fn entries(&self, kind: &str) -> i64 {
        self.number(&format!(
            "SELECT {}(*) FROM {}.__pbps_state WHERE kind='{kind}'",
            self.count(),
            self.ledger()
        ))
    }
    fn rows(&self, table: &str, value: &str) {
        assert_eq!(
            self.number(&format!("SELECT {}(*) FROM {table}", self.count())),
            2
        );
        assert_eq!(
            self.number(&format!(
                "SELECT {}(*) FROM {table} WHERE id=1 AND label='{value}'",
                self.count()
            )),
            1
        );
        assert_eq!(
            self.number(&format!(
                "SELECT {}(*) FROM {table} WHERE id=2 AND label='sentinel'",
                self.count()
            )),
            1
        );
    }
}

fn succeeds(out: Output) {
    assert!(
        out.status.success(),
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

#[cfg(target_os = "linux")]
struct CheckpointOutput {
    reader: std::io::PipeReader,
    writer: Option<std::io::PipeWriter>,
    capture: File,
    filler: usize,
    armed: bool,
}

#[cfg(target_os = "linux")]
impl CheckpointOutput {
    fn drain(&mut self) {
        use std::io::{Read, Write};
        assert!(!self.armed, "draining an armed gate would release the CLI");
        let mut buf = [0; 8192];
        loop {
            match self.reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    let skip = n.min(self.filler);
                    assert!(buf[..skip].iter().all(|b| *b == 0));
                    self.filler -= skip;
                    self.capture.write_all(&buf[skip..n]).unwrap();
                    assert!(self.capture.metadata().unwrap().len() < 1024 * 1024);
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(e) => panic!("stdout read: {e}"),
            }
        }
    }

    fn arm(&mut self) {
        use rustix::fs::{OFlags, fcntl_getfl, fcntl_setfl};
        use std::io::Write;
        self.drain();
        let writer = self.writer.as_mut().unwrap();
        let flags = fcntl_getfl(&*writer).unwrap();
        fcntl_setfl(&*writer, flags | OFlags::NONBLOCK).unwrap();
        // Single-byte writes prove there is no space even for the smallest
        // next output. This happens once per qualification, outside production.
        loop {
            match writer.write(&[0]) {
                Ok(1) => self.filler += 1,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                other => panic!("fill stdout: {other:?}"),
            }
        }
        assert!(self.filler > 0);
        fcntl_setfl(&*writer, flags).unwrap();
        self.armed = true;
    }

    fn release(&mut self) {
        self.writer.take();
        self.armed = false;
        self.drain();
    }
}

#[cfg(target_os = "linux")]
impl Running {
    fn await_pipe_write(&mut self) {
        // A full pipe alone is not a receipt: without this witness a fast
        // observer can see checkpoint 1 while the CLI is already advancing.
        // The main thread executes apply through Runtime::block_on; stdout is
        // its only pipe (stderr is a file). An unreadable/hidden wait channel
        // is failed fixture setup, never permission to use a timing guess.
        let wait = format!("/proc/{}/wchan", self.child.id());
        let start = Instant::now();
        loop {
            assert!(self.child.try_wait().unwrap().is_none());
            let channel = std::fs::read_to_string(&wait).expect("owned child wait channel");
            if channel.trim().ends_with("pipe_write") {
                return;
            }
            assert!(
                start.elapsed() < LIMIT,
                "CLI never blocked writing its full stdout pipe: {channel}"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn checkpoint(d: &super::Demo, args: &[&str]) -> Self {
        use rustix::fs::{OFlags, fcntl_getfl, fcntl_setfl};
        let (reader, writer) = std::io::pipe().unwrap();
        let child_writer = writer.try_clone().unwrap();
        fcntl_setfl(&reader, fcntl_getfl(&reader).unwrap() | OFlags::NONBLOCK).unwrap();
        let stdout = d.dir.join("interrupted.stdout");
        let stderr = d.dir.join("interrupted.stderr");
        let capture = File::create(&stdout).unwrap();
        let mut running = Self::start_with_output(d, args, child_writer.into(), stdout, stderr);
        running.pipe = Some(CheckpointOutput {
            reader,
            writer: Some(writer),
            capture,
            filler: 0,
            armed: false,
        });
        running
    }
}
