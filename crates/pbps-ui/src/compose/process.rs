//! Bounded, noninteractive children. Diagnostics are not credential-safe data.

use std::io::{Read, Write};
use std::process::{Child, Command, Output, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use super::{Error, Result};

pub(super) const OUTPUT_LIMIT: u64 = 64 * 1024 * 1024;

fn read(mut stream: impl Read) -> std::io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    stream
        .by_ref()
        .take(OUTPUT_LIMIT + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > OUTPUT_LIMIT {
        return Err(std::io::Error::other(
            "subprocess output exceeds compose limit",
        ));
    }
    Ok(bytes)
}

fn kill(child: &mut Child) {
    if let Some(pid) = rustix::process::Pid::from_raw(child.id() as i32) {
        let _ = rustix::process::kill_process_group(pid, rustix::process::Signal::KILL);
    }
    let _ = child.kill();
    let _ = child.wait();
}

pub(super) fn run(mut command: Command, input: &[u8], deadline: Duration) -> Result<Output> {
    use std::os::unix::process::CommandExt as _;
    command
        .process_group(0)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command
        .spawn()
        .map_err(|_| Error::new("Could not start the compose subprocess"))?;
    let mut stdin = child.stdin.take().expect("piped stdin");
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");
    let bytes = input.to_vec();
    let (send, receive) = mpsc::channel();
    let output_send = send.clone();
    std::thread::spawn(move || {
        let _ = output_send.send((0, read(stdout)));
    });
    let error_send = send.clone();
    std::thread::spawn(move || {
        let _ = error_send.send((1, read(stderr)));
    });
    std::thread::spawn(move || {
        let written = stdin.write_all(&bytes).map(|()| Vec::new());
        drop(stdin);
        let _ = send.send((2, written));
    });
    let started = Instant::now();
    let mut streams: [Option<Vec<u8>>; 3] = [None, None, None];
    // Block on the readers rather than polling: a child that finishes in a
    // millisecond must not wait out a fixed interval, and a compose operation
    // runs hundreds of them (#1539).
    while streams.iter().any(Option::is_none) {
        // The deadline includes pipe drainage: a helper can retain a pipe
        // after its parent exits. Joining readers first would wait forever.
        let Some(remaining) = left(started, deadline) else {
            kill(&mut child);
            return Err(Error::new("Compose subprocess exceeded its deadline"));
        };
        match receive.recv_timeout(remaining) {
            Ok((which, Ok(bytes))) => streams[which] = Some(bytes),
            Err(mpsc::RecvTimeoutError::Timeout) => (),
            Ok((_, Err(_))) | Err(mpsc::RecvTimeoutError::Disconnected) => {
                kill(&mut child);
                return Err(Error::new(
                    "Could not exchange bounded compose subprocess data",
                ));
            }
        }
    }
    match reaped(&mut child, started, deadline) {
        Ok(Some(status)) => Ok(Output {
            status,
            stdout: streams[0].take().expect("read stdout"),
            stderr: streams[1].take().expect("read stderr"),
        }),
        Ok(None) => {
            kill(&mut child);
            Err(Error::new("Compose subprocess exceeded its deadline"))
        }
        Err(_) => {
            kill(&mut child);
            Err(Error::new("Could not determine compose subprocess outcome"))
        }
    }
}

fn left(started: Instant, deadline: Duration) -> Option<Duration> {
    deadline
        .checked_sub(started.elapsed())
        .filter(|d| !d.is_zero())
}

/// The child's status once its pipes have closed, or `None` at the deadline.
/// Closing them at exit makes the first poll the usual answer; the backoff
/// covers a child that closed its pipes and lives on, without spinning.
fn reaped(
    child: &mut Child,
    started: Instant,
    deadline: Duration,
) -> std::io::Result<Option<std::process::ExitStatus>> {
    let mut pause = Duration::from_micros(50);
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(Some(status));
        }
        let Some(remaining) = left(started, deadline) else {
            return Ok(None);
        };
        std::thread::sleep(pause.min(remaining));
        pause = (pause * 2).min(Duration::from_millis(5));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_deadline_includes_pipes_held_after_the_parent_exits() {
        let mut command = Command::new("sh");
        command.args(["-c", "sleep 30 & exit 0"]);
        let start = Instant::now();
        let error = run(command, &[], Duration::from_millis(100)).unwrap_err();
        assert!(error.to_string().contains("deadline"));
        assert!(start.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn a_quick_child_returns_without_waiting_out_a_polling_interval() {
        // A fixed 5 ms poll made every run take at least 5 ms (#1539). The
        // fastest of twenty is robust to a loaded host, which slows some
        // spawns but not all of them.
        let fastest = (0..20)
            .map(|_| {
                let start = Instant::now();
                let output = run(Command::new("true"), &[], Duration::from_secs(5)).unwrap();
                assert!(output.status.success());
                start.elapsed()
            })
            .min()
            .unwrap();
        assert!(fastest < Duration::from_millis(5), "{fastest:?}");
    }

    #[test]
    fn a_child_living_on_after_closing_its_pipes_still_meets_the_deadline() {
        // The pipes close first, so the wait for the status is the only
        // thing left to bound.
        let mut command = Command::new("sh");
        command.args(["-c", "exec >&- 2>&- <&-; sleep 30"]);
        let start = Instant::now();
        let error = run(command, &[], Duration::from_millis(100)).unwrap_err();
        assert!(error.to_string().contains("deadline"));
        assert!(start.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn a_failing_child_is_reported_with_its_status_not_as_success() {
        let mut command = Command::new("sh");
        command.args(["-c", "printf out; printf err >&2; exit 3"]);
        let output = run(command, &[], Duration::from_secs(5)).unwrap();
        assert_eq!(output.status.code(), Some(3));
        assert_eq!(output.stdout, b"out");
        assert_eq!(output.stderr, b"err");
    }

    #[test]
    fn bidirectional_pipes_do_not_deadlock_and_missing_processes_are_not_success() {
        let bytes = vec![b'x'; 1024 * 1024];
        let output = run(Command::new("cat"), &bytes, Duration::from_secs(5)).unwrap();
        assert!(output.status.success());
        assert_eq!(output.stdout, bytes);
        assert!(
            run(
                Command::new("/nonexistent/pbps-compose-test"),
                &[],
                Duration::from_secs(1)
            )
            .is_err()
        );
    }
}

/// A bounded exchange for Git's prepared ref transaction. The caller sends
/// only fixed protocol commands, with a total input budget below a pipe page.
pub(super) struct Transaction {
    child: Option<Child>,
    input: Option<std::process::ChildStdin>,
    events: mpsc::Receiver<TransactionEvent>,
    started: Instant,
    deadline: Duration,
    sent: usize,
    stdout_closed: bool,
    stderr_closed: bool,
}

enum TransactionEvent {
    Line(Vec<u8>),
    StdoutClosed,
    StderrClosed,
    Failed,
}

impl Transaction {
    pub fn start(mut command: Command, deadline: Duration) -> Result<Self> {
        use std::io::BufRead as _;
        use std::os::unix::process::CommandExt as _;
        command
            .process_group(0)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command
            .spawn()
            .map_err(|_| Error::new("Cannot start the ref transaction"))?;
        let input = child.stdin.take();
        let stdout = child.stdout.take().expect("piped stdout");
        let stderr = child.stderr.take().expect("piped stderr");
        let (sender, events) = mpsc::sync_channel(16);
        let errors = sender.clone();
        std::thread::spawn(move || {
            let event = if read(stderr).is_ok() {
                TransactionEvent::StderrClosed
            } else {
                TransactionEvent::Failed
            };
            let _ = errors.send(event);
        });
        std::thread::spawn(move || {
            let mut reader = std::io::BufReader::new(stdout);
            loop {
                let mut line = Vec::new();
                let result = reader.by_ref().take(8193).read_until(b'\n', &mut line);
                let event = match result {
                    Ok(0) => TransactionEvent::StdoutClosed,
                    Ok(_) if line.len() <= 8192 && line.ends_with(b"\n") => {
                        TransactionEvent::Line(line)
                    }
                    _ => TransactionEvent::Failed,
                };
                let end = !matches!(event, TransactionEvent::Line(_));
                if sender.send(event).is_err() || end {
                    break;
                }
            }
        });
        Ok(Self {
            child: Some(child),
            input,
            events,
            started: Instant::now(),
            deadline,
            sent: 0,
            stdout_closed: false,
            stderr_closed: false,
        })
    }

    fn remaining(&self) -> Result<Duration> {
        self.deadline
            .checked_sub(self.started.elapsed())
            .filter(|d| !d.is_zero())
            .ok_or_else(|| Error::new("Compose ref transaction exceeded its deadline"))
    }

    fn event(&mut self) -> Result<Option<Vec<u8>>> {
        let remaining = self.remaining()?;
        match self
            .events
            .recv_timeout(remaining.min(Duration::from_millis(5)))
        {
            Ok(TransactionEvent::Line(line)) => Ok(Some(line)),
            Ok(TransactionEvent::StdoutClosed) => {
                self.stdout_closed = true;
                Ok(None)
            }
            Ok(TransactionEvent::StderrClosed) => {
                self.stderr_closed = true;
                Ok(None)
            }
            Err(mpsc::RecvTimeoutError::Timeout) => Ok(None),
            Err(mpsc::RecvTimeoutError::Disconnected)
                if self.stdout_closed && self.stderr_closed =>
            {
                std::thread::sleep(remaining.min(Duration::from_millis(5)));
                Ok(None)
            }
            _ => Err(Error::new("Compose ref transaction exchange failed")),
        }
    }

    pub fn exchange(&mut self, input: &str, expected: &[u8]) -> Result<()> {
        self.remaining()?;
        self.sent += input.len();
        if self.sent > 4096 || self.stdout_closed {
            return Err(Error::new("Invalid ref transaction exchange"));
        }
        self.input
            .as_mut()
            .ok_or_else(|| Error::new("The ref transaction is closed"))?
            .write_all(input.as_bytes())
            .map_err(|_| Error::new("Ref transaction acknowledgement is unavailable"))?;
        loop {
            if let Some(line) = self.event()? {
                return if line == expected {
                    Ok(())
                } else {
                    Err(Error::new("Unexpected ref transaction acknowledgement"))
                };
            }
            if self.stdout_closed {
                return Err(Error::new("Ref transaction ended before acknowledgement"));
            }
        }
    }

    pub fn finish(mut self) -> Result<()> {
        self.input.take();
        loop {
            let status = self
                .child
                .as_mut()
                .expect("owned child")
                .try_wait()
                .map_err(|_| Error::new("Cannot determine ref transaction outcome"))?;
            if let Some(status) = status
                && self.stdout_closed
                && self.stderr_closed
            {
                self.child.take();
                return if status.success() {
                    Ok(())
                } else {
                    Err(Error::new("Git refused the ref transaction"))
                };
            }
            if self.event()?.is_some() {
                return Err(Error::new("Unexpected trailing ref transaction output"));
            }
        }
    }
}

impl Drop for Transaction {
    fn drop(&mut self) {
        self.input.take();
        if let Some(child) = &mut self.child {
            kill(child);
        }
    }
}

#[cfg(test)]
mod transaction_tests {
    use super::*;

    #[test]
    fn interactive_deadline_includes_a_helper_retaining_the_acknowledgement_pipe() {
        let mut command = Command::new("sh");
        command.args(["-c", "read line; printf 'start: ok\\n'; sleep 30 & exit 0"]);
        let start = Instant::now();
        let mut transaction = Transaction::start(command, Duration::from_millis(100)).unwrap();
        transaction.exchange("start\n", b"start: ok\n").unwrap();
        assert!(
            transaction
                .finish()
                .unwrap_err()
                .to_string()
                .contains("deadline")
        );
        assert!(start.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn interactive_exchange_requires_exact_acknowledgements_and_bounded_input() {
        let mut transaction =
            Transaction::start(Command::new("cat"), Duration::from_secs(2)).unwrap();
        transaction.exchange("start\n", b"start\n").unwrap();
        assert!(transaction.exchange("prepare\n", b"prepare: ok\n").is_err());
        let mut transaction =
            Transaction::start(Command::new("cat"), Duration::from_secs(2)).unwrap();
        assert!(
            transaction
                .exchange(&"x".repeat(4097), b"unused\n")
                .is_err()
        );
        assert!(
            Transaction::start(
                Command::new("/nonexistent/pbps-transaction"),
                Duration::from_secs(1)
            )
            .is_err()
        );
    }
}
