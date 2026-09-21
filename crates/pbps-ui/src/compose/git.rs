//! Every `git` the compose runs, run the one way ADR-0015 decision 5 permits.
//!
//! The decision's refusals are only as good as the process they run in: a hook
//! that pushes, a `core.fsmonitor` program, a pathspec `git` reads as a
//! pattern, a `GIT_DIR` inherited from whatever shell started `pbps ui`, or a
//! credential helper that blocks with no terminal each defeat a check that is
//! otherwise correct. So no caller builds a `Command` of its own; they all
//! come through [`Git::run`], which applies the whole list.

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// Repository-locating variables of the UI's own environment.
///
/// Removed from every child, because `pbps ui` may have been started from a
/// shell that set one: the repository the compose acts on is the one found
/// from the worktree root, never one a variable points at. `GIT_INDEX_FILE` is
/// on the list *and* set deliberately by step 3 — [`Git::run_with_index`]
/// re-adds it after the scrub, so an inherited value cannot survive by
/// accident while a chosen one still works.
///
/// The list is exact rather than "every `GIT_*`": `GIT_SSH_COMMAND` and the
/// credential configuration are how the user's own push authenticates, and a
/// commit the UI makes is meant to be the commit the shell would have made
/// (ADR-0015 decision 5's reason for refusing a library).
const REPOSITORY_LOCATING: &[&str] = &[
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_COMMON_DIR",
    "GIT_INDEX_FILE",
    "GIT_INDEX_VERSION",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_CEILING_DIRECTORIES",
    "GIT_DISCOVERY_ACROSS_FILESYSTEM",
    "GIT_NAMESPACE",
    "GIT_PREFIX",
    "GIT_ATTR_SOURCE",
    "GIT_CONFIG",
    "GIT_CONFIG_GLOBAL",
    "GIT_CONFIG_SYSTEM",
    "GIT_CONFIG_NOSYSTEM",
    "GIT_CONFIG_COUNT",
];

/// What a `git` run produced, or why it produced nothing.
#[derive(Debug)]
pub struct Run {
    pub code: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

impl Run {
    pub fn ok(&self) -> bool {
        self.code == Some(0)
    }

    /// `stdout` as text with the trailing newline removed, for the commands
    /// that answer with one line (`rev-parse`, `symbolic-ref`, `config`).
    ///
    /// Not for anything that prints paths: those take `-z` and are parsed as
    /// bytes, because `git` C-quotes a non-ASCII name without it.
    pub fn line(&self) -> Result<String, Failure> {
        let text = std::str::from_utf8(&self.stdout)
            .map_err(|_| Failure::Output("git printed bytes that are not UTF-8".to_owned()))?;
        Ok(text.trim_end_matches(['\n', '\r']).to_owned())
    }
}

/// A `git` that did not answer. Distinct from a `git` that answered "no": a
/// refusal the UI reports is a decision, and a subprocess that never ran is
/// not one (AGENTS.md — absent, empty and unreadable are three different
/// things).
#[derive(Debug)]
pub enum Failure {
    /// `git` could not be started at all.
    Unstartable(String),
    /// The deadline expired and the process group was killed.
    Deadline { command: String, after: Duration },
    /// `git` answered, but not in the shape the caller has to be able to read.
    Output(String),
}

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unstartable(detail) => write!(f, "could not run git: {detail}"),
            Self::Deadline { command, after } => write!(
                f,
                "git {command} did not finish within {}s and was stopped",
                after.as_secs()
            ),
            Self::Output(detail) => write!(f, "git answered unusably: {detail}"),
        }
    }
}

/// The environment and flags every compose `git` shares.
pub struct Git {
    /// `git rev-parse --show-toplevel`. Every command runs here and every path
    /// is spelled relative to it: a pathspec resolves from the current
    /// directory while `--cacheinfo` takes an index name from the root, so one
    /// spelling in two places is two different files.
    root: PathBuf,
    /// An empty directory. `-c core.hooksPath` points at it so that the hooks
    /// plumbing still runs — `reference-transaction`, `post-index-change` —
    /// find nothing to run, rather than the UI having to know which command
    /// runs which hook.
    hooks: PathBuf,
    /// A program that exits non-zero. Taking the terminal away is not enough:
    /// a helper can put up a window of its own or block with no terminal.
    askpass: PathBuf,
    deadline: Duration,
}

impl Git {
    /// `private` is a directory the UI owns; the empty hooks directory and the
    /// askpass program are made inside it.
    pub fn new(root: PathBuf, private: &Path, deadline: Duration) -> std::io::Result<Self> {
        let hooks = private.join("no-hooks");
        std::fs::create_dir_all(&hooks)?;
        let askpass = private.join("refuse-askpass");
        write_refusing_program(&askpass)?;
        Ok(Self {
            root,
            hooks,
            askpass,
            deadline,
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn run<S: AsRef<OsStr>>(&self, arguments: &[S]) -> Result<Run, Failure> {
        self.spawn(arguments, None, &BTreeMap::new(), None)
    }

    /// Step 3's private index, and step 6's locked copy of the user's.
    pub fn run_with_index<S: AsRef<OsStr>>(
        &self,
        index: &Path,
        arguments: &[S],
    ) -> Result<Run, Failure> {
        self.spawn(arguments, Some(index), &BTreeMap::new(), None)
    }

    /// The push and its `ls-remote`, whose remote exists only here: a URL may
    /// carry a credential, and a command line is readable by every user of the
    /// machine through `/proc/<pid>/cmdline` where the environment is readable
    /// by the process's owner alone.
    pub fn run_with_environment<S: AsRef<OsStr>>(
        &self,
        extra: &BTreeMap<String, OsString>,
        arguments: &[S],
    ) -> Result<Run, Failure> {
        self.spawn(arguments, None, extra, None)
    }

    /// A `git` the caller speaks to a line at a time, for the one command that
    /// needs it: `update-ref --stdin`, whose transaction must be prepared,
    /// then questioned while the lock is held, and only then committed.
    pub fn interactive<S: AsRef<OsStr>>(&self, arguments: &[S]) -> Result<Session, Failure> {
        let mut command = Command::new("git");
        self.dress(&mut command, None, &BTreeMap::new());
        command.args(arguments.iter().map(AsRef::as_ref));
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command
            .spawn()
            .map_err(|e| Failure::Unstartable(e.to_string()))?;
        let stdin = child.stdin.take().expect("stdin was piped");
        let mut stdout = child.stdout.take().expect("stdout was piped");
        let mut stderr = child.stderr.take().expect("stderr was piped");
        let (send, lines) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut reader = std::io::BufReader::new(&mut stdout);
            loop {
                let mut line = String::new();
                match std::io::BufRead::read_line(&mut reader, &mut line) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {
                        if send.send(line).is_err() {
                            break;
                        }
                    }
                }
            }
        });
        let errors = std::thread::spawn(move || {
            let mut bytes = Vec::new();
            let _ = stderr.read_to_end(&mut bytes);
            bytes
        });
        Ok(Session {
            child,
            stdin: Some(stdin),
            lines,
            errors: Some(errors),
            deadline: self.deadline,
            printable: printable(arguments),
        })
    }

    fn spawn<S: AsRef<OsStr>>(
        &self,
        arguments: &[S],
        index: Option<&Path>,
        extra: &BTreeMap<String, OsString>,
        stdin: Option<&[u8]>,
    ) -> Result<Run, Failure> {
        let mut command = Command::new("git");
        self.dress(&mut command, index, extra);
        command.args(arguments.iter().map(AsRef::as_ref));
        let printable = printable(arguments);
        command
            .stdin(if stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command
            .spawn()
            .map_err(|e| Failure::Unstartable(e.to_string()))?;
        if let Some(bytes) = stdin {
            let mut handle = child.stdin.take().expect("stdin was piped");
            use std::io::Write as _;
            // A `git` that refuses before reading all of it closes the pipe;
            // that is its answer, not an error of ours.
            let _ = handle.write_all(bytes);
            drop(handle);
        }
        self.collect(child, &printable)
    }

    fn dress(
        &self,
        command: &mut Command,
        index: Option<&Path>,
        extra: &BTreeMap<String, OsString>,
    ) {
        for name in REPOSITORY_LOCATING {
            command.env_remove(name);
        }
        // `GIT_CONFIG_COUNT` is on the list above, so any inherited numbered
        // entries are inert; removing the ones this process can see keeps the
        // child's environment honest for anyone reading it.
        for (name, _) in std::env::vars_os().filter_map(|(k, v)| Some((k.into_string().ok()?, v))) {
            if name.starts_with("GIT_CONFIG_KEY_") || name.starts_with("GIT_CONFIG_VALUE_") {
                command.env_remove(&name);
            }
        }
        command
            .current_dir(&self.root)
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_ASKPASS", &self.askpass)
            .env("SSH_ASKPASS", &self.askpass)
            .env("SSH_ASKPASS_REQUIRE", "never");
        if let Some(path) = index {
            command.env("GIT_INDEX_FILE", path);
        }
        for (name, value) in extra {
            command.env(name, value);
        }
        command.args([
            OsStr::new("-C"),
            self.root.as_os_str(),
            OsStr::new("-c"),
            &hooks_setting(&self.hooks),
            OsStr::new("-c"),
            OsStr::new("core.fsmonitor=false"),
            OsStr::new("--literal-pathspecs"),
            OsStr::new("--no-replace-objects"),
        ]);
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt as _;
            // Its own group, so the deadline can take the whole tree with it:
            // a `git push` that hangs has an ssh under it, and killing the
            // parent alone leaves the child holding the terminal-less prompt.
            command.process_group(0);
        }
    }

    fn collect(&self, mut child: Child, printable: &str) -> Result<Run, Failure> {
        let mut out = child.stdout.take().expect("stdout was piped");
        let mut err = child.stderr.take().expect("stderr was piped");
        // Drained on threads: `cat-file --batch` fills a pipe long before it
        // exits, and a reader that waits for the exit first would deadlock
        // against a writer waiting for the pipe.
        let reader = std::thread::spawn(move || {
            let mut bytes = Vec::new();
            let _ = out.read_to_end(&mut bytes);
            bytes
        });
        let errors = std::thread::spawn(move || {
            let mut bytes = Vec::new();
            let _ = err.read_to_end(&mut bytes);
            bytes
        });
        let started = Instant::now();
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break Some(status),
                Ok(None) => {}
                Err(e) => return Err(Failure::Unstartable(e.to_string())),
            }
            if started.elapsed() >= self.deadline {
                kill_group(&mut child);
                break None;
            }
            std::thread::sleep(Duration::from_millis(5));
        };
        let stdout = reader.join().unwrap_or_default();
        let stderr = errors.join().unwrap_or_default();
        match status {
            Some(status) => Ok(Run {
                code: status.code(),
                stdout,
                stderr,
            }),
            None => Err(Failure::Deadline {
                command: printable.to_owned(),
                after: self.deadline,
            }),
        }
    }
}

/// A `git` held open, spoken to a line at a time.
///
/// It exists for `update-ref --stdin`, where the whole point is the interval
/// between `prepare` and `commit`: the transaction holds the branch's lock,
/// and the UI asks the *live* ref whether it is still direct before letting
/// the write through. A one-shot command cannot ask a question in the middle
/// of itself.
#[derive(Debug)]
pub struct Session {
    child: Child,
    stdin: Option<std::process::ChildStdin>,
    lines: std::sync::mpsc::Receiver<String>,
    errors: Option<std::thread::JoinHandle<Vec<u8>>>,
    deadline: Duration,
    printable: String,
}

impl Session {
    pub fn send(&mut self, text: &str) -> Result<(), Failure> {
        use std::io::Write as _;
        let stdin = self
            .stdin
            .as_mut()
            .ok_or_else(|| Failure::Output("the transaction's input is closed".to_owned()))?;
        stdin
            .write_all(text.as_bytes())
            .and_then(|()| stdin.flush())
            .map_err(|e| Failure::Output(e.to_string()))
    }

    /// One line of `git`'s answer, or the deadline. A transaction that never
    /// answers is killed with its process group like any other subprocess:
    /// what it is holding is a ref lock, and waiting on it forever would hold
    /// the checkout too.
    pub fn line(&mut self) -> Result<String, Failure> {
        match self.lines.recv_timeout(self.deadline) {
            Ok(line) => Ok(line.trim_end_matches(['\n', '\r']).to_owned()),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                kill_group(&mut self.child);
                Err(Failure::Deadline {
                    command: self.printable.clone(),
                    after: self.deadline,
                })
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => Err(Failure::Output(
                "the transaction ended without answering".to_owned(),
            )),
        }
    }

    /// Close the input and wait. The UI waits for the acknowledgement *and*
    /// the process exit before retaking its own locks, because the
    /// transaction releases `git`'s locks on its way out and a lock taken
    /// before that is a lock taken against `git` itself.
    pub fn finish(mut self) -> Result<Run, Failure> {
        self.stdin = None;
        let started = Instant::now();
        let status = loop {
            match self.child.try_wait() {
                Ok(Some(status)) => break Some(status),
                Ok(None) => {}
                Err(e) => return Err(Failure::Unstartable(e.to_string())),
            }
            if started.elapsed() >= self.deadline {
                kill_group(&mut self.child);
                break None;
            }
            std::thread::sleep(Duration::from_millis(5));
        };
        let stdout = self.lines.try_iter().collect::<Vec<_>>().join("");
        let stderr = self
            .errors
            .take()
            .map(|errors| errors.join().unwrap_or_default())
            .unwrap_or_default();
        match status {
            Some(status) => Ok(Run {
                code: status.code(),
                stdout: stdout.into_bytes(),
                stderr,
            }),
            None => Err(Failure::Deadline {
                command: self.printable.clone(),
                after: self.deadline,
            }),
        }
    }
}

fn hooks_setting(hooks: &Path) -> OsString {
    let mut setting = OsString::from("core.hooksPath=");
    setting.push(hooks);
    setting
}

fn printable<S: AsRef<OsStr>>(arguments: &[S]) -> String {
    arguments
        .iter()
        .map(|a| a.as_ref().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(unix)]
fn kill_group(child: &mut Child) {
    use rustix::process::{Pid, Signal, kill_process_group};
    if let Some(pid) = Pid::from_raw(child.id() as i32) {
        let _ = kill_process_group(pid, Signal::KILL);
    }
    let _ = child.kill();
    let _ = child.wait();
}

#[cfg(not(unix))]
fn kill_group(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

#[cfg(unix)]
fn write_refusing_program(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    // Its own program rather than `/bin/false`, so that the refusal does not
    // depend on a path outside the repository's control, and `#!/bin/sh` so
    // that it needs no compiler at launch.
    std::fs::write(path, "#!/bin/sh\nexit 1\n")?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
}

#[cfg(not(unix))]
fn write_refusing_program(path: &Path) -> std::io::Result<()> {
    std::fs::write(path, "exit 1\r\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_scrubbed_list_names_every_variable_that_could_relocate_the_repository() {
        // The point of the list is what is *on* it. `GIT_DIR` and
        // `GIT_WORK_TREE` are the obvious two; the ones that cost a measured
        // surprise are the object directories and `GIT_INDEX_FILE`, which the
        // compose sets itself and must never inherit.
        for name in [
            "GIT_DIR",
            "GIT_WORK_TREE",
            "GIT_COMMON_DIR",
            "GIT_INDEX_FILE",
            "GIT_OBJECT_DIRECTORY",
            "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        ] {
            assert!(
                REPOSITORY_LOCATING.contains(&name),
                "{name} could relocate the repository and is not scrubbed"
            );
        }
    }

    #[test]
    fn the_scrubbed_list_leaves_the_users_own_transport_configuration_alone() {
        // A commit and a push the UI makes are meant to be the ones the shell
        // would have made; taking the user's ssh command away would make them
        // authenticate differently, which is the difference an auditor asks
        // about (ADR-0015 decision 5's reason for refusing a library).
        for name in ["GIT_SSH_COMMAND", "GIT_SSH", "GIT_AUTHOR_NAME"] {
            assert!(
                !REPOSITORY_LOCATING.contains(&name),
                "{name} is the user's own configuration and must survive"
            );
        }
    }

    #[test]
    fn a_line_answer_keeps_no_trailing_newline_and_refuses_bytes_that_are_not_text() {
        let run = Run {
            code: Some(0),
            stdout: b"refs/heads/main\n".to_vec(),
            stderr: Vec::new(),
        };
        assert_eq!(run.line().unwrap(), "refs/heads/main");
        let binary = Run {
            code: Some(0),
            stdout: vec![0xff, 0xfe],
            stderr: Vec::new(),
        };
        assert!(binary.line().is_err());
    }
}
