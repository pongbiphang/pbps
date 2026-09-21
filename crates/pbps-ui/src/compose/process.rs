//! Bounded, noninteractive children. Diagnostics are not credential-safe data.

use std::io::{Read, Write};
use std::process::{Child, Command, Output, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use super::{Error, Result};

const OUTPUT_LIMIT: u64 = 64 * 1024 * 1024;

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
    let mut status = None;
    let mut streams: [Option<Vec<u8>>; 3] = [None, None, None];
    loop {
        while let Ok((which, result)) = receive.try_recv() {
            match result {
                Ok(bytes) => streams[which] = Some(bytes),
                Err(_) => {
                    kill(&mut child);
                    return Err(Error::new(
                        "Could not exchange bounded compose subprocess data",
                    ));
                }
            }
        }
        if status.is_none() {
            match child.try_wait() {
                Ok(value) => status = value,
                Err(_) => {
                    kill(&mut child);
                    return Err(Error::new("Could not determine compose subprocess outcome"));
                }
            }
        }
        if let Some(status) = status
            && streams.iter().all(Option::is_some)
        {
            return Ok(Output {
                status,
                stdout: streams[0].take().expect("read stdout"),
                stderr: streams[1].take().expect("read stderr"),
            });
        }
        // The deadline includes pipe drainage: a helper can retain a pipe
        // after its parent exits. Joining readers first would wait forever.
        if started.elapsed() >= deadline {
            kill(&mut child);
            return Err(Error::new("Compose subprocess exceeded its deadline"));
        }
        std::thread::sleep(Duration::from_millis(5));
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
