#!/usr/bin/env python3
"""Admit a pinned local/CI fixture before any test creates database objects."""

import argparse
import json
import os
from pathlib import Path
import subprocess
import sys
import time


MATRIX = json.loads(Path(__file__).with_name("compatibility-matrix.json").read_text())


def validate_container(row, container, image, port):
    # A manifest digest is not an image/config ID. Resolve the pinned manifest
    # through Docker, then compare the concrete running image with that ID.
    if not container["State"]["Running"]:
        raise ValueError("fixture is not running")
    if image["Os"] != "linux" or image["Architecture"] != "amd64":
        raise ValueError("fixture requires the qualified linux/amd64 image")
    if container["Image"] != image["Id"]:
        raise ValueError("running fixture image differs from the pinned image")
    bindings = container["NetworkSettings"]["Ports"].get(f'{row["port"]}/tcp')
    if not bindings or any(
        str(binding["HostPort"]) != str(port)
        or binding["HostIp"] not in ("", "0.0.0.0", "127.0.0.1", "::", "::1")
        for binding in bindings
    ):
        raise ValueError("fixture port is not the expected localhost endpoint")


def validate_server(row, fields):
    expected = [row["version"]]
    if row["engine"] == "mssql":
        expected += [row["edition"], str(row["engine_edition"]), row["collation"]]
    if fields != expected:
        raise ValueError(f"server identity differs: expected {expected!r}, observed {fields!r}")


def docker(*args):
    result = subprocess.run(["docker", *args], capture_output=True, text=True, timeout=45)
    if result.returncode:
        # Neither a failed inspect nor a failed query is an absent fixture.
        # Do not echo the command: the readiness query carries a fixture secret.
        raise RuntimeError("Docker fixture inspection/query failed; inspect the named fixture")
    return result.stdout


# Exit status of `--survive` for a start-up crash: the one case the caller may
# answer with a fresh start (DEC-2875.1). Any other outcome exits 0 or 1.
CRASHED = 75


def survive(container, seconds, *, clock=time.monotonic, sleep=time.sleep):
    """Hold a fixture that answered its readiness probe for `seconds`.

    Returns ("survived", "") when it was still running at the end of the
    window, ("crash", log) when it exited and its log names a `core.sqlservr`
    dump, and ("other", "") for every other way of not running, including a
    container the runtime no longer knows. Only a crash proves nothing about
    the code under test (DEC-958.1); the rest is left to the admission below
    to refuse.
    """
    deadline = clock() + seconds
    while True:
        try:
            running = docker("container", "inspect", "--format", "{{.State.Running}}", container).strip()
        except (RuntimeError, subprocess.TimeoutExpired):
            return "other", ""
        if running != "true":
            break
        if clock() >= deadline:
            return "survived", ""
        sleep(1)
    try:
        # The engine writes to both streams, and an unreadable log proves nothing.
        done = subprocess.run(["docker", "logs", container], capture_output=True, text=True, timeout=45)
    except subprocess.TimeoutExpired:
        return "other", ""
    log = done.stdout + done.stderr
    if done.returncode == 0 and "core.sqlservr" in log:
        return "crash", log
    return "other", ""


# Long enough to reach past a crash dump's own report: the dump collector's
# `find: Permission denied` lines alone filled 80 lines (DEC-958.1).
TAIL = 300


def report(container, log):
    """Print what the crashed fixture left behind: it is removed next."""
    print(f"--- {container}: crashed while starting; the end of its log ---", flush=True)
    for line in log.splitlines()[-TAIL:]:
        print(f"  {line}", flush=True)
    print(f"--- {container}: ends ---", flush=True)


def inspect(kind, name):
    values = json.loads(docker(kind, "inspect", name))
    if len(values) != 1:
        raise ValueError("fixture inspection must identify exactly one object")
    return values[0]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--cell", choices=MATRIX)
    parser.add_argument("--container", required=True)
    parser.add_argument("--port", type=int)
    parser.add_argument("--survive", type=float, metavar="SECONDS",
                        help="only hold the container for this long and exit %d if it "
                             "crashed with a core dump (DEC-2875.1)" % CRASHED)
    args = parser.parse_args()
    if args.survive is not None:
        if args.survive < 0:
            parser.error("--survive must not be negative")
        verdict, log = survive(args.container, args.survive)
        if verdict == "crash":
            report(args.container, log)
            sys.exit(CRASHED)
        return
    if args.cell is None or args.port is None:
        parser.error("--cell and --port are required unless --survive is given")
    if not 1 <= args.port <= 65535:
        parser.error("port must be between 1 and 65535")
    row = MATRIX[args.cell]
    container = inspect("container", args.container)
    image = inspect("image", row["image"])
    validate_container(row, container, image, args.port)
    if row["engine"] == "postgres":
        text = docker("exec", "-e", "PGPASSWORD=" + os.environ.get("PBPS_FIXTURE_PASSWORD", "Pbps!Test12345"),
                      args.container, "psql", "-h", "127.0.0.1", "-U", "postgres", "-d", "postgres",
                      "-Atc", "SELECT current_setting('server_version_num')")
    else:
        text = docker("exec", args.container, "/opt/mssql-tools18/bin/sqlcmd", "-C",
                      "-S", "localhost", "-U", "sa", "-P",
                      os.environ.get("PBPS_FIXTURE_PASSWORD", "Pbps!Test12345"),
                      "-b", "-h", "-1", "-W", "-s", "|", "-Q",
                      "SET NOCOUNT ON; SELECT CONVERT(varchar(128),SERVERPROPERTY('ProductVersion')),"
                      "CONVERT(varchar(128),SERVERPROPERTY('Edition')),"
                      "CONVERT(int,SERVERPROPERTY('EngineEdition')),"
                      "CONVERT(varchar(128),SERVERPROPERTY('Collation'));")
    fields = text.strip().split("|")
    validate_server(row, fields)
    print(json.dumps({"cell": args.cell, "container": container["Id"],
                      "image": image["Id"], "pin": row["image"],
                      "localhost_port": args.port, "server": fields}))


if __name__ == "__main__":
    main()
