#!/usr/bin/env python3
"""Measure socket activation in one disposable, private PID namespace."""

import json
from pathlib import Path
import subprocess
import tarfile
import tempfile
import uuid


IMAGE = "postgres@sha256:4ef4dbc939d61acea57712655ddb4b4ab27419c913f94cca0cd57cb3ea3c2280"
TEST = "resolver::native::daemon::tests::socket_activation_requires_the_candidate_daemon_to_own_the_actual_peer"


def main():
    built = subprocess.run(["cargo", "test", "-p", "pbps-cli", "--lib", "--no-run",
                            "--message-format=json"], check=True, text=True,
                           stdout=subprocess.PIPE)
    binaries = []
    for line in built.stdout.splitlines():
        item = json.loads(line)
        if item.get("reason") == "compiler-artifact" and item.get("executable"):
            binaries.append(item["executable"])
    if len(binaries) != 1:
        raise RuntimeError("expected exactly one library fixture executable")
    name = "pbps-daemon-fixture-" + uuid.uuid4().hex
    try:
        # Synthetic executables and PID file stay inside this fixture. No
        # host namespaces, mounts, runtime socket or additional capabilities.
        subprocess.run(["docker", "create", "--name", name, "--pull", "never",
                        "--network", "none", "--cap-drop", "ALL",
                        "--security-opt", "no-new-privileges", "--memory", "256m",
                        "--cpus", "1", "--pids-limit", "32", "--user", "0",
                        "-e", "PBPS_DAEMON_FIXTURE=1", "--entrypoint", "/systemd",
                        IMAGE, "--ignored", "--exact", TEST, "--nocapture"],
                       check=True, stdout=subprocess.DEVNULL)
        # Explicit archive ownership models root-installed executables even
        # when the local Docker copy path preserves the builder's UID.
        with tempfile.TemporaryFile() as archive:
            with tarfile.open(fileobj=archive, mode="w") as files:
                info = files.gettarinfo(str(Path(binaries[0])), arcname="systemd")
                info.uid = info.gid = 0
                info.uname = info.gname = "root"
                info.mode = 0o755
                with open(binaries[0], "rb") as source:
                    files.addfile(info, source)
                alias = tarfile.TarInfo("dockerd")
                alias.mode = 0o755
                alias.uname = alias.gname = "root"
                alias.type = tarfile.LNKTYPE
                alias.linkname = "systemd"
                files.addfile(alias)
            archive.seek(0)
            subprocess.run(["docker", "cp", "-", f"{name}:/"], stdin=archive,
                           check=True)
        result = subprocess.run(["docker", "start", "--attach", name],
                                text=True, stdout=subprocess.PIPE,
                                stderr=subprocess.STDOUT, timeout=30)
        print(result.stdout, end="", flush=True)
        if result.returncode or "test result: ok. 1 passed" not in result.stdout:
            raise RuntimeError("socket-activation fixture must pass")
    finally:
        subprocess.run(["docker", "rm", "--force", "--volumes", name], check=True,
                       stdout=subprocess.DEVNULL)


if __name__ == "__main__":
    main()
