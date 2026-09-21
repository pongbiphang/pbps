#!/usr/bin/env python3
"""Exercise namespace observations on disposable rootful Linux fixtures.

The runtime owns two private PID namespaces. A daemon-exec sleeper has an
external parent; a native helper retains a worker after its leader exits.
Mount-substitution negatives run in another fresh mount/PID namespace.
No existing container, host mount, or runtime configuration is modified.
"""
import argparse
import json
import os
from pathlib import Path
import signal
import subprocess
import sys
import tempfile
import time
import uuid

IMAGE = "postgres@sha256:4ef4dbc939d61acea57712655ddb4b4ab27419c913f94cca0cd57cb3ea3c2280"
PREFIX = "resolver::native::namespace::tests::"
PROC_TEST = PREFIX + "namespace_procfs_fixture_observes_external_parent_tasks_and_retains_identity"
MOUNT_TEST = PREFIX + "namespace_mount_fixture_refuses_replaced_views_and_process_overmounts"
NEGATIVE_TEST = PREFIX + "container_membership_does_not_hide_wrong_credentials_or_foreign_cgroups"
CHURN_TEST = PREFIX + "departing_incidental_tasks_do_not_refuse_an_unchanged_container_profile"
ORPHAN_TEST = PREFIX + "reparenting_during_qualification_keeps_the_held_grandchild"
FOREIGN_TEST = PREFIX + "foreign_namespace_sharers_remain_visible_outside_the_container_pid_view"


def run(*args, **kwargs):
    try:
        return subprocess.run(args, check=True, text=True, **kwargs)
    except subprocess.CalledProcessError as error:
        if error.stdout:
            print(error.stdout, file=sys.stderr)
        if error.stderr:
            print(error.stderr, file=sys.stderr)
        raise


def test(binary, name, env, prefix=()):
    result = run(*prefix, str(binary), "--exact", name, "--nocapture", env=env,
                 stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
    print(result.stdout, end="", flush=True)
    if "1 passed; 0 failed" not in result.stdout:
        raise RuntimeError("the selected fixture test did not run")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--test-binary", required=True, type=Path)
    parser.add_argument("--runtime", choices=("docker", "podman"), default="docker")
    parser.add_argument("--runtime-binary")
    parser.add_argument("--docker-socket", default="/var/run/docker.sock")
    parser.add_argument("--image", default=IMAGE)
    args = parser.parse_args()
    if os.geteuid() != 0:
        parser.error("this fixture requires a root observer and a rootful runtime")
    binary = args.test_binary.resolve(strict=True)
    command = [args.runtime_binary or args.runtime]
    if args.runtime == "docker":
        if not os.path.isabs(args.docker_socket):
            parser.error("the Docker socket must be absolute")
        command += ["--host", "unix://" + args.docker_socket]

    def runtime(*arguments):
        return run(*command, *arguments, stdout=subprocess.PIPE, stderr=subprocess.PIPE).stdout.strip()

    def interrupted(signum, _frame):
        raise SystemExit(128 + signum)

    for sig in (signal.SIGINT, signal.SIGTERM):
        signal.signal(sig, interrupted)
    owned = []
    try:
        print(json.dumps({"runtime": args.runtime, "image": runtime("image", "inspect", args.image, "--format", "{{.Id}}")}), flush=True)
        with tempfile.TemporaryDirectory(prefix="pbps-namespace-") as temporary:
            helper = Path(temporary) / "thread-exit"
            run("cc", "-static", "-pthread", "-Wall", "-Wextra", "-Werror", "-o", str(helper),
                str(Path(__file__).parent / "fixtures/namespace-thread.c"))
            pids = []
            for _ in range(2):
                name = "pbps-namespace-" + uuid.uuid4().hex
                owned.append(name)
                runtime("create", "--name", name, "--pull=never", "--network=none", "--ipc=private",
                        "--read-only", "--user=999:999", "--cap-drop=ALL", "--security-opt=no-new-privileges",
                        "--mount", f"type=bind,src={helper},dst=/pbps-thread-exit,readonly",
                        "--entrypoint=sleep", args.image, "300")
                runtime("start", name)
                pids.append(int(runtime("inspect", "--format", "{{.State.Pid}}", name)))
            runtime("exec", "-d", owned[0], "sleep", "299")
            runtime("exec", "-d", owned[0], "/pbps-thread-exit")
            view = Path(f"/proc/{pids[0]}/root/proc")
            dead_group = None
            for _ in range(100):
                for entry in view.iterdir():
                    if not entry.name.isdecimal():
                        continue
                    try:
                        fields = (entry / "stat").read_text().rsplit(")", 1)[1].split()
                    except FileNotFoundError:
                        continue
                    if fields[0] == "Z" and int(fields[17]) > 1:
                        dead_group = entry.name
                        break
                if dead_group:
                    break
                time.sleep(.05)
            if not dead_group:
                raise RuntimeError("the native helper did not retain a live thread after leader exit")
            test(binary, PROC_TEST, dict(os.environ, PBPS_NAMESPACE_FIXTURE_PID=str(pids[0]),
                 PBPS_NAMESPACE_SECOND_PID=str(pids[1]), PBPS_NAMESPACE_DEAD_GROUP=dead_group))
            for namespace, option in [("mnt", "--mount"), ("ipc", "--ipc")]:
                sharer = subprocess.Popen(["nsenter", "--target", str(pids[0]), option, "--", "sleep", "296"])
                try:
                    expected = os.readlink(f"/proc/{pids[0]}/ns/{namespace}")
                    for _ in range(100):
                        if (os.readlink(f"/proc/{sharer.pid}/ns/{namespace}") == expected
                                and Path(f"/proc/{sharer.pid}/comm").read_text().strip() == "sleep"):
                            break
                        time.sleep(.01)
                    else:
                        raise RuntimeError("foreign namespace sharer did not become ready")
                    test(binary, FOREIGN_TEST, dict(os.environ, PBPS_NAMESPACE_FOREIGN_PID=str(pids[0]),
                         PBPS_NAMESPACE_FOREIGN_KIND=namespace))
                finally:
                    sharer.terminate()
                    sharer.wait(timeout=10)
            runtime("exec", "-d", owned[0], "/pbps-thread-exit", "churn")
            for _ in range(100):
                try:
                    ready = any((p / "comm").read_text().strip() == "pbps-churn"
                                for p in view.iterdir() if p.name.isdecimal())
                except FileNotFoundError:
                    ready = False
                if ready:
                    break
                time.sleep(.01)
            else:
                raise RuntimeError("the churn helper did not become ready")
            test(binary, CHURN_TEST, dict(os.environ, PBPS_NAMESPACE_CHURN_PID=str(pids[0])))
            # Runtime exec has a parent outside init's descendant tree. Its
            # root credentials must nevertheless participate in admission.
            runtime("exec", "-d", "--user=0:0", owned[0], "sleep", "298")
            test(binary, NEGATIVE_TEST, dict(os.environ, PBPS_NAMESPACE_NEGATIVE_PID=str(pids[0]),
                 PBPS_NAMESPACE_NEGATIVE="credentials"))
            runtime("exec", "-d", "--privileged", "--user=0:0", owned[1], "/pbps-thread-exit", "mixed")
            second_view = Path(f"/proc/{pids[1]}/root/proc")
            for _ in range(100):
                if any((p / "comm").read_text().strip() == "pbps-mixed"
                       for p in second_view.iterdir() if p.name.isdecimal()):
                    break
                time.sleep(.01)
            else:
                raise RuntimeError("the mixed-credential worker did not become ready")
            test(binary, NEGATIVE_TEST, dict(os.environ, PBPS_NAMESPACE_NEGATIVE_PID=str(pids[1]),
                 PBPS_NAMESPACE_NEGATIVE="thread-credentials"))
            # Enter only the fixture's PID namespace, retaining our own
            # cgroup. A cgroup-sourced census would omit this actual member.
            before = {p.name for p in second_view.iterdir() if p.name.isdecimal()}
            entrant = subprocess.Popen(["nsenter", "--target", str(pids[1]), "--pid", "--", "sleep", "297"])
            try:
                # The mixed-credential helper is already a second group.
                for _ in range(100):
                    if {p.name for p in second_view.iterdir() if p.name.isdecimal()} - before:
                        break
                    time.sleep(.01)
                else:
                    raise RuntimeError("outside-cgroup task did not enter the fixture namespace")
                test(binary, NEGATIVE_TEST, dict(os.environ, PBPS_NAMESPACE_NEGATIVE_PID=str(pids[1]),
                     PBPS_NAMESPACE_NEGATIVE="cgroup"))
            finally:
                # nsenter waits for its PID-namespace child; stopping only
                # nsenter could orphan the sleeper. Remove our container to
                # terminate every task in that private namespace, then reap.
                runtime("stop", "--time=0", owned[1])
                entrant.wait(timeout=10)
            orphan = "pbps-namespace-" + uuid.uuid4().hex
            owned.append(orphan)
            runtime("run", "-d", "--name", orphan, "--pull=never", "--network=none", "--ipc=private",
                    "--read-only", "--user=999:999", "--cap-drop=ALL", "--security-opt=no-new-privileges",
                    "--mount", f"type=bind,src={helper},dst=/pbps-thread-exit,readonly",
                    "--entrypoint=/pbps-thread-exit", args.image, "orphan")
            orphan_pid = runtime("inspect", "--format", "{{.State.Pid}}", orphan)
            ready = Path(f"/proc/{orphan_pid}/root/dev/shm/ready")
            for _ in range(100):
                if ready.exists() and len(ready.read_text().split()) == 2:
                    break
                time.sleep(.01)
            else:
                raise RuntimeError("the orphan fixture did not become ready")
            test(binary, ORPHAN_TEST, dict(os.environ, PBPS_NAMESPACE_ORPHAN_PID=orphan_pid))
            test(binary, MOUNT_TEST, dict(os.environ, PBPS_NAMESPACE_MOUNT_FIXTURE="1"),
                 prefix=("unshare", "--mount", "--pid", "--fork", "--mount-proc", "--propagation", "private"))
    finally:
        remaining = []
        for name in reversed(owned):
            removed = subprocess.run([*command, "rm", "-f", name], text=True,
                                     stdout=subprocess.PIPE, stderr=subprocess.PIPE)
            if removed.returncode:
                print(removed.stderr, file=sys.stderr)
                remaining.append(name)
        if remaining:
            raise RuntimeError("fixture cleanup could not confirm removal: " + ", ".join(remaining))


if __name__ == "__main__":
    main()
