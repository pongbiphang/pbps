#!/usr/bin/env python3
"""What a fixture container leaves behind, printed before it is removed.

These fixtures remove every container they own, so whatever is not printed
here is gone. A start that fails is the case that matters: `ControlLost` and
`did not become ready` are what an engine that crashed while starting looks
like from outside, and without the container's own log they cannot be told
from a fault in the code under test. In a merge queue that is a pull request
ejected with nothing to read (#724).

**An empty log and a log nobody read are printed differently**, which is the
whole reason this exists as its own module rather than a line at each site.
The occurrence that filed the issue printed `did not become ready (exited
exit=1)` followed by nothing at all, and nothing at all is ambiguous: it reads
as "the fixture does not print logs" when it in fact read one and found it
empty. Absent, empty and unreadable are three different things here too.
"""

import argparse
from pathlib import Path
import subprocess
import uuid

TAIL = "80"


def report(run, container):
    """Print `container`'s state and log. Never raises: this is what runs on
    the way to an error, and losing the error to a second one loses both."""
    print(f"--- {container}: what it left behind ---", flush=True)
    print(f"state: {_state(run, container)}", flush=True)
    for line in _log(run, container).splitlines() or ["(the log is empty)"]:
        print(f"  {line}", flush=True)
    print(f"--- {container}: ends ---", flush=True)


def _state(run, container):
    try:
        done = run("docker", "inspect", "--format",
                   "{{.State.Status}} exit={{.State.ExitCode}} oom={{.State.OOMKilled}}"
                   " error={{printf \"%q\" .State.Error}}",
                   container, stdout=subprocess.PIPE, stderr=subprocess.PIPE, check=False)
    except OSError as error:
        return f"(unreadable: {error})"
    if done.returncode:
        return f"(unreadable: {(done.stderr or '').strip() or 'exit ' + str(done.returncode)})"
    return done.stdout.strip() or "(the daemon said nothing)"


def _log(run, container):
    try:
        done = run("docker", "logs", "--tail", TAIL, container, stdout=subprocess.PIPE,
                   stderr=subprocess.PIPE, check=False)
    except OSError as error:
        return f"(the log is unreadable: {error})"
    if done.returncode:
        reason = (done.stderr or "").strip() or f"exit {done.returncode}"
        return f"(the log is unreadable: {reason})"
    # The engine writes to both, and which one carries the crash is the
    # engine's business rather than this fixture's.
    return (done.stdout or "") + (done.stderr or "")


def self_check(run, image):
    """Start a container whose engine exits at once, and require that the
    report carries its output. The fixtures print this on the path to an
    error, so a reporter that silently printed nothing would be discovered
    only by the failure it was added to explain (#724)."""
    name = "pbps-fixture-diagnostics-" + uuid.uuid4().hex[:12]
    marker = "pbps-self-check-" + uuid.uuid4().hex[:12]
    run("docker", "create", "--name", name, "--pull", "never", "--network", "none",
        "--entrypoint", "/bin/sh", image, "-c", f"echo {marker}; exit 3",
        check=True, stdout=subprocess.DEVNULL)
    try:
        run("docker", "start", "-a", name, check=False, stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL)
        captured = _capture(run, name)
        if marker not in captured:
            raise SystemExit(
                "the fixture reporter lost a failed container's own output:\n" + captured)
        if "exit=3" not in captured:
            raise SystemExit("the fixture reporter lost the exit status:\n" + captured)
        # And the other half of "absent, empty and unreadable": a name the
        # daemon does not have must say so rather than print an empty log.
        absent = _capture(run, name + "-does-not-exist")
        if "unreadable" not in absent:
            raise SystemExit("an absent container read as an empty log:\n" + absent)
    finally:
        run("docker", "rm", "-f", name, check=False, stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL)
    left = run("docker", "ps", "-a", "--filter", "name=" + name, "--format", "{{.Names}}",
               check=False, stdout=subprocess.PIPE).stdout.strip()
    if left:
        raise SystemExit("the self check left a container behind: " + left)
    print("fixture diagnostics: a failed container's log and status are reported", flush=True)


def _capture(run, container):
    import io
    import contextlib
    buffer = io.StringIO()
    with contextlib.redirect_stdout(buffer):
        report(run, container)
    print(buffer.getvalue(), end="", flush=True)
    return buffer.getvalue()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--self-check", action="store_true", required=True)
    parser.add_argument("--socket", default="/var/run/docker.sock")
    parser.add_argument("--image", required=True)
    args = parser.parse_args()
    socket = Path(args.socket)
    if not socket.is_absolute():
        parser.error("the selected Docker socket must be absolute")

    def run(*args_, **kwargs):
        if args_[0] == "docker":
            args_ = ("docker", "--host", "unix://" + str(socket), *args_[1:])
        kwargs.setdefault("text", True)
        return subprocess.run(args_, **kwargs)

    self_check(run, args.image)


if __name__ == "__main__":
    main()
