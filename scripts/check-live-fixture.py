#!/usr/bin/env python3
"""Admit a pinned local/CI fixture before any test creates database objects."""

import argparse
import json
import os
from pathlib import Path
import subprocess


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


def inspect(kind, name):
    values = json.loads(docker(kind, "inspect", name))
    if len(values) != 1:
        raise ValueError("fixture inspection must identify exactly one object")
    return values[0]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--cell", choices=MATRIX, required=True)
    parser.add_argument("--container", required=True)
    parser.add_argument("--port", type=int, required=True)
    args = parser.parse_args()
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
