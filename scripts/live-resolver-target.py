#!/usr/bin/env python3
"""Verify native target identity using disposable engine and TLS fixtures.

Default mode confines the inspector to the owned target's PID/network
namespaces. --native-host exercises the public Docker factory on a disposable
native Linux runner; it requires root and a direct native Docker daemon.
"""

import argparse
from fixture_diagnostics import report
import json
import os
from pathlib import Path
import signal
import subprocess
import sys
import tempfile
import time
import uuid


IMAGES = {
    "pg": "postgres@sha256:4ef4dbc939d61acea57712655ddb4b4ab27419c913f94cca0cd57cb3ea3c2280",
    "mssql": "mcr.microsoft.com/mssql/server@sha256:4bab24f36c1ecd48e85f7d37df26e6bf301641d84c3fe652f9a0dcc947d512e1",
}
PASSWORD = "Pbps!NativeFixture12345"
TARGET_TEST = "resolver::native::target::tests::native_aliases_share_one_instance_and_backend_children_cannot_claim_another"
FACTORY_TEST = "resolver::docker::session::native_factory_tests::native_factory_qualifies_before_bootstrap_and_rejects_rebound_target_connections"
QUIET = {"stdout": subprocess.DEVNULL, "stderr": subprocess.DEVNULL}
DOCKER_SOCKET = "/var/run/docker.sock"


def run(*args, **kwargs):
    if args[0] == "docker":
        args = ("docker", "--host", "unix://" + DOCKER_SOCKET, *args[1:])
    kwargs.setdefault("check", True)
    return subprocess.run(args, text=True, **kwargs)


def interrupted(signum, _frame):
    raise SystemExit(128 + signum)


def test_binary():
    built = run("cargo", "test", "-p", "pbps-cli", "--lib", "--no-run",
                "--message-format=json", stdout=subprocess.PIPE)
    artifacts = [json.loads(line) for line in built.stdout.splitlines()]
    return next(item["executable"] for item in artifacts
                if item.get("executable") and item.get("target", {}).get("name") == "pbps_cli")


def certificates(root):
    run("openssl", "req", "-x509", "-newkey", "rsa:2048", "-nodes", "-days", "2",
        "-subj", "/CN=pbps native test CA", "-keyout", str(root / "ca.key"),
        "-out", str(root / "ca.pem"), **QUIET)
    run("openssl", "req", "-newkey", "rsa:2048", "-nodes", "-subj", "/CN=localhost",
        "-keyout", str(root / "peer.key"), "-out", str(root / "peer.csr"), **QUIET)
    (root / "extensions").write_text(
        "subjectAltName=DNS:localhost,IP:127.0.0.1\nbasicConstraints=CA:FALSE\n"
        "keyUsage=digitalSignature,keyEncipherment\nextendedKeyUsage=serverAuth\n")
    run("openssl", "x509", "-req", "-in", str(root / "peer.csr"), "-CA", str(root / "ca.pem"),
        "-CAkey", str(root / "ca.key"), "-CAcreateserial", "-days", "2", "-extfile",
        str(root / "extensions"), "-out", str(root / "peer.pem"), **QUIET)
    (root / "empty-ca").mkdir()


def fixture(args, binary, root, owned):
    certificates(root)
    engine = args.engine
    name = "pbps-native-target-" + uuid.uuid4().hex
    if engine == "pg":
        environment = ["-e", f"POSTGRES_PASSWORD={PASSWORD}"]
        boot = ("chown postgres:postgres /tmp/peer.key; chmod 600 /tmp/peer.key; "
                "exec docker-entrypoint.sh postgres -c listen_addresses=127.0.0.1 "
                "-c ssl=on -c ssl_cert_file=/tmp/peer.pem -c ssl_key_file=/tmp/peer.key")
        probe = ["pg_isready", "-h", "127.0.0.1", "-U", "postgres"]
    else:
        environment = ["-e", "ACCEPT_EULA=Y", "-e", f"MSSQL_SA_PASSWORD={PASSWORD}",
                       "-e", "MSSQL_MEMORY_LIMIT_MB=1024"]
        boot = ("chown mssql:root /tmp/peer.key; chmod 600 /tmp/peer.key; "
                "exec su -s /bin/bash mssql -c /opt/mssql/bin/sqlservr")
        probe = ["/opt/mssql-tools18/bin/sqlcmd", "-C", "-S", "127.0.0.1", "-U", "sa",
                 "-P", PASSWORD, "-Q", "SELECT 1"]
        (root / "mssql.conf").write_text(
            "[network]\nipaddress=127.0.0.1\ntlscert=/tmp/peer.pem\n"
            "tlskey=/tmp/peer.key\nforceencryption=0\n")
    # Record the exact generated name before create so interrupted replies
    # cannot lose its cleanup scope. No shared test container is modified.
    owned.append(name)
    target = run("docker", "create", "--name", name, "--pull", "never", "--network",
                 "host" if args.native_host else "none", "--user", "0", "--memory", "3g",
                 "--cpus", "2", "--pids-limit", "512", *environment,
                 "--entrypoint", "/bin/bash", IMAGES[engine], "-ec", boot,
                 stdout=subprocess.PIPE).stdout.strip()
    for leaf in ("peer.key", "peer.pem"):
        run("docker", "cp", str(root / leaf), target + ":/tmp/" + leaf, **QUIET)
    if engine == "mssql":
        run("docker", "cp", str(root / "mssql.conf"), target + ":/var/opt/mssql/mssql.conf", **QUIET)
    run("docker", "start", target, **QUIET)
    for _ in range(60):
        if run("docker", "exec", target, *probe, check=False, **QUIET).returncode == 0:
            break
        time.sleep(1)
    else:
        # This said only that it failed, and the cleanup then removed the
        # container: an engine that crashed while starting and a fault in the
        # fixture read identically (#724).
        report(run, target)
        raise RuntimeError("owned native TLS fixture failed to start")
    trust = str(root / "ca.pem") if args.native_host else "/tmp/ca.pem"
    if engine == "pg":
        statements = ["CREATE DATABASE pbps_native_alias",
                      f"CREATE ROLE pbps_native_alt LOGIN PASSWORD '{PASSWORD}'",
                      "GRANT EXECUTE ON FUNCTION pg_catalog.pg_control_system() TO pbps_native_alt"]
        for statement in statements:
            run("docker", "exec", "-e", f"PGPASSWORD={PASSWORD}", target, "psql", "-h",
                "127.0.0.1", "-U", "postgres", "-v", "ON_ERROR_STOP=1", "-c", statement, **QUIET)
        primary = f"host=localhost port=5432 user=postgres password={PASSWORD} dbname=postgres sslmode=require"
        alias = f"host=127.0.0.1 port=5432 user=pbps_native_alt password={PASSWORD} dbname=pbps_native_alias sslmode=require"
        executable = "postgres"
    else:
        batches = [
            ("master", "CREATE DATABASE pbps_native_alias"),
            ("master", f"CREATE LOGIN pbps_native_alt WITH PASSWORD='{PASSWORD}', CHECK_POLICY=OFF; "
             "GRANT VIEW ANY DATABASE TO pbps_native_alt; CREATE USER pbps_native_alt FOR LOGIN pbps_native_alt"),
            ("pbps_native_alias", "CREATE USER pbps_native_alt FOR LOGIN pbps_native_alt"),
        ]
        for database, sql in batches:
            run("docker", "exec", target, "/opt/mssql-tools18/bin/sqlcmd", "-C", "-S", "127.0.0.1",
                "-U", "sa", "-P", PASSWORD, "-d", database, "-b", "-Q", sql, **QUIET)
        primary = f"Server=localhost,1433;User Id=sa;Password={PASSWORD};Database=master;Encrypt=true;TrustServerCertificateCA={trust}"
        alias = f"Server=127.0.0.1,1433;User Id=pbps_native_alt;Password={PASSWORD};Database=pbps_native_alias;Encrypt=true;TrustServerCertificateCA={trust}"
        executable = "sqlservr"
    ps = (["docker", "top", target, "-eo", "pid,ppid,comm"] if args.native_host
          else ["docker", "exec", target, "/bin/ps", "-eo", "pid,ppid,comm"])
    rows = [line.split() for line in run(*ps, stdout=subprocess.PIPE).stdout.splitlines()[1:]]
    processes = {row[0] for row in rows if row[2] == executable}
    roots = [row[0] for row in rows if row[0] in processes and row[1] not in processes]
    if len(roots) != 1:
        raise RuntimeError("ambiguous owned fixture service")
    env = dict(PBPS_NATIVE_DRIVER=engine, PBPS_NATIVE_SERVICE_PID=roots[0],
               PBPS_NATIVE_CONNECTION=primary, PBPS_NATIVE_ALIAS_CONNECTION=alias,
               SSL_CERT_FILE=trust,
               SSL_CERT_DIR=str(root / "empty-ca") if args.native_host else "/tmp/empty-ca")
    env["PATH"] = "/pbps-no-external-tools"
    env["PBPS_NATIVE_PROXY_CERT"] = str(root / "peer.pem") if args.native_host else "/tmp/proxy.pem"
    env["PBPS_NATIVE_PROXY_KEY"] = str(root / "peer.key") if args.native_host else "/tmp/proxy.key"
    if args.native_host:
        env.update(PBPS_NATIVE_FACTORY_FIXTURE="1", PBPS_RESOLVER_TEST_SOCKET=args.socket,
                   PBPS_RESOLVER_TEST_IMAGE=IMAGES[engine])
        for test in (TARGET_TEST, FACTORY_TEST):
            result = run(binary, "--ignored", "--exact", test, "--nocapture",
                         env=dict(os.environ, **env), stdout=subprocess.PIPE, stderr=subprocess.STDOUT, check=False)
            print(result.stdout, end="", flush=True)
            if result.returncode or "test result: ok. 1 passed" not in result.stdout:
                raise RuntimeError("native fixture did not run exactly one passing test")
        return
    reader = "pbps-native-reader-" + uuid.uuid4().hex
    owned.append(reader)
    variables = [arg for key, value in env.items() for arg in ("-e", key + "=" + value)]
    run("docker", "create", "--name", reader, "--pull", "never", "--network", "container:" + target,
        "--pid", "container:" + target, "--cap-drop", "ALL", "--cap-add", "SYS_PTRACE", "--cap-add",
        "DAC_READ_SEARCH", "--cap-add", "KILL", "--security-opt", "no-new-privileges", "--memory",
        "768m", "--cpus", "1", "--pids-limit", "256", *variables, "--entrypoint", "/usr/bin/timeout",
        IMAGES["pg"], "--signal=KILL", "45s", "/pbps-cli-tests", "--ignored", "--exact",
        TARGET_TEST, "--nocapture", **QUIET)
    run("docker", "cp", binary, reader + ":/pbps-cli-tests", **QUIET)
    run("docker", "cp", str(root / "ca.pem"), reader + ":/tmp/ca.pem", **QUIET)
    run("docker", "cp", str(root / "peer.pem"), reader + ":/tmp/proxy.pem", **QUIET)
    run("docker", "cp", str(root / "peer.key"), reader + ":/tmp/proxy.key", **QUIET)
    result = run("docker", "start", "--attach", reader, stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
                 timeout=60, check=False)
    print(result.stdout, end="", flush=True)
    status = run("docker", "inspect", "--format", "{{.State.ExitCode}}", reader,
                 stdout=subprocess.PIPE).stdout.strip()
    if result.returncode or status != "0" or "test result: ok. 1 passed" not in result.stdout:
        raise RuntimeError("native target fixture failed")


def main():
    global DOCKER_SOCKET
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("engine", choices=IMAGES)
    parser.add_argument("--native-host", action="store_true")
    parser.add_argument("--socket", default="/var/run/docker.sock")
    parser.add_argument("--test-binary", type=Path, help="prebuilt CLI library test executable")
    args = parser.parse_args()
    if sys.platform != "linux":
        parser.error("native process qualification requires Linux")
    if not Path(args.socket).is_absolute():
        parser.error("the Docker socket must be absolute")
    DOCKER_SOCKET = args.socket
    if args.native_host and os.geteuid() != 0:
        parser.error("--native-host requires root on an explicitly disposable native Linux runner")
    signal.signal(signal.SIGINT, interrupted)
    signal.signal(signal.SIGTERM, interrupted)
    binary = str(args.test_binary.resolve()) if args.test_binary else test_binary()
    owned = []
    try:
        with tempfile.TemporaryDirectory(prefix="pbps-native-pki-") as directory:
            fixture(args, binary, Path(directory), owned)
    finally:
        for resource in reversed(owned):
            run("docker", "rm", "--force", "--volumes", resource, stdout=subprocess.DEVNULL)


if __name__ == "__main__":
    main()
