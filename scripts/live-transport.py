#!/usr/bin/env python3
"""Test the peer-verified TLS hop using an owned engine and disposable PKI.

This fixture is deliberately separate from runtime containment/admission. It
never alters a shared test server's TLS configuration or the host trust store.
"""

import argparse
import json
import os
from pathlib import Path
import signal
import subprocess
import tempfile
import time
import uuid


IMAGES = {
    "pg": "postgres@sha256:4ef4dbc939d61acea57712655ddb4b4ab27419c913f94cca0cd57cb3ea3c2280",
    "mssql": "mcr.microsoft.com/mssql/server@sha256:4bab24f36c1ecd48e85f7d37df26e6bf301641d84c3fe652f9a0dcc947d512e1",
}
PASSWORD = "Pbps!Test12345"


def run(*args, **kwargs):
    return subprocess.run(args, check=True, text=True, **kwargs)


def interrupted(signum, _frame):
    raise SystemExit(128 + signum)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("engine", choices=IMAGES)
    args = parser.parse_args()
    signal.signal(signal.SIGTERM, interrupted)
    signal.signal(signal.SIGINT, interrupted)
    with tempfile.TemporaryDirectory(prefix="pbps-tls-") as directory:
        root = Path(directory)
        (root / "empty").mkdir()
        quiet = {"stdout": subprocess.DEVNULL, "stderr": subprocess.DEVNULL}
        for name in ("ca", "untrusted"):
            run("openssl", "req", "-x509", "-newkey", "rsa:2048", "-nodes",
                "-days", "2", "-subj", f"/CN=pbps disposable {name}",
                "-keyout", str(root / f"{name}.key"),
                "-out", str(root / f"{name}.pem"), **quiet)
        run("openssl", "req", "-newkey", "rsa:2048", "-nodes",
            "-subj", "/CN=localhost", "-keyout", str(root / "peer.key"),
            "-out", str(root / "peer.csr"), **quiet)
        (root / "extensions").write_text(
            "subjectAltName=DNS:localhost\nbasicConstraints=CA:FALSE\n"
            "keyUsage=digitalSignature,keyEncipherment\nextendedKeyUsage=serverAuth\n"
        )
        run("openssl", "x509", "-req", "-in", str(root / "peer.csr"),
            "-CA", str(root / "ca.pem"), "-CAkey", str(root / "ca.key"),
            "-CAcreateserial", "-days", "2", "-extfile", str(root / "extensions"),
            "-out", str(root / "peer.pem"), **quiet)
        (root / "invalid.pem").write_text("not a certificate\n")
        name = "pbps-tls-" + uuid.uuid4().hex
        internal_port = "5432" if args.engine == "pg" else "1433"
        if args.engine == "pg":
            environment = ["-e", f"POSTGRES_PASSWORD={PASSWORD}"]
            command = (
                "chown postgres:postgres /tmp/pbps-peer.key; "
                "chmod 600 /tmp/pbps-peer.key; "
                "exec docker-entrypoint.sh postgres -c ssl=on "
                "-c ssl_cert_file=/tmp/pbps-peer.pem -c ssl_key_file=/tmp/pbps-peer.key"
            )
            probe = ["pg_isready", "-U", "postgres"]
        else:
            environment = ["-e", "ACCEPT_EULA=Y", "-e", f"MSSQL_SA_PASSWORD={PASSWORD}"]
            command = (
                "chown mssql:root /tmp/pbps-peer.key; chmod 600 /tmp/pbps-peer.key; "
                "exec su -s /bin/bash mssql -c /opt/mssql/bin/sqlservr"
            )
            probe = ["/opt/mssql-tools18/bin/sqlcmd", "-C", "-S", "localhost",
                     "-U", "sa", "-P", PASSWORD, "-Q", "SELECT 1"]
            (root / "mssql.conf").write_text(
                "[network]\ntlscert = /tmp/pbps-peer.pem\n"
                "tlskey = /tmp/pbps-peer.key\nforceencryption = 1\n"
            )
        # The unguessable name is owned by this invocation, including a create
        # interrupted after Docker accepted it but before returning its ID.
        try:
            run("docker", "create", "--name", name, "--user", "0", "--memory", "3g",
                "-p", f"127.0.0.1::{internal_port}", *environment,
                "--entrypoint", "/bin/bash", IMAGES[args.engine], "-ec", command, **quiet)
            for file in ("peer.key", "peer.pem"):
                run("docker", "cp", str(root / file), f"{name}:/tmp/pbps-{file}", **quiet)
            if args.engine == "mssql":
                run("docker", "cp", str(root / "mssql.conf"), f"{name}:/var/opt/mssql/mssql.conf", **quiet)
            run("docker", "start", name, **quiet)
            for _ in range(60):
                check = subprocess.run(["docker", "exec", name, *probe], **quiet)
                if check.returncode == 0:
                    break
                time.sleep(2)
            else:
                run("docker", "logs", name)
                raise RuntimeError("TLS fixture did not become ready")
            ports = json.loads(run("docker", "inspect", "--format",
                                   "{{json .NetworkSettings.Ports}}", name,
                                   capture_output=True).stdout)
            env = dict(os.environ, PBPS_TEST_TLS_ENGINE=args.engine,
                       PBPS_TEST_TLS_PORT=ports[internal_port + "/tcp"][0]["HostPort"],
                       PBPS_TEST_TLS_CA=str(root / "ca.pem"),
                       SSL_CERT_FILE=str(root / "ca.pem"), SSL_CERT_DIR=str(root / "empty"))
            cargo = ["cargo", "test", "-p", "pbps-db", "--test", "live_transport", "--",
                     "--ignored", "--test-threads=1"]
            run(*cargo, "verified_round_trips_reject_wrong_peers_and_corrupted_replies", env=env)
            for trust in ("untrusted.pem", "invalid.pem", "missing.pem"):
                env["SSL_CERT_FILE"] = str(root / trust)
                run(*cargo, "invalid_trust_cannot_yield_a_verified_connection", env=env)
        finally:
            # Do not silently ignore failed cleanup: no shared container is
            # named, and a failed removal leaves this fixture's resources live.
            run("docker", "rm", "-fv", name, stdout=subprocess.DEVNULL)


if __name__ == "__main__":
    main()
