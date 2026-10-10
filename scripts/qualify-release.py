#!/usr/bin/env python3
"""Qualify an already-built release binary in an owned, source-free consumer.

The host provisions fixtures; only the artifact, generated project and trust
files enter the consumer. Git remains a runtime prerequisite for provenance.
"""

import argparse
from contextlib import contextmanager
import hashlib
import json
import os
from pathlib import Path
import platform
import re
import shutil
import signal
import subprocess
import tempfile
import time
import uuid


PASSWORD = "Pbps!Test12345"
IMAGES = {
    "postgres": "postgres@sha256:4ef4dbc939d61acea57712655ddb4b4ab27419c913f94cca0cd57cb3ea3c2280",
    "mssql": "mcr.microsoft.com/mssql/server@sha256:4bab24f36c1ecd48e85f7d37df26e6bf301641d84c3fe652f9a0dcc947d512e1",
}
CASES = (
    "artifact-identity", "offline", "trusted-bootstrap", "wrong-peer",
    "untrusted-root", "restored-trust", "connected-plan", "wrong-checksum",
    "unchanged-after-refusal", "apply", "verify", "ledger",
)


def run(*args, codes=(0,), timeout=90, **kwargs):
    try:
        result = subprocess.run(args, text=True, encoding="utf-8", errors="replace",
                                capture_output=True, timeout=timeout,
                                stdin=subprocess.DEVNULL, **kwargs)
    except subprocess.TimeoutExpired:
        raise RuntimeError(f"{Path(args[0]).name} exceeded {timeout}s deadline") from None
    if result.returncode not in codes:
        # Fixture credentials are disposable, but never teach a runner to echo
        # connection strings or arbitrary command environments on failures.
        detail = (result.stdout + result.stderr)[-6000:].replace(PASSWORD, "[redacted]")
        raise RuntimeError(f"{Path(args[0]).name} exited {result.returncode}: {detail}")
    return result


def digest(path):
    with Path(path).open("rb") as source:
        return hashlib.file_digest(source, "sha256").hexdigest()


def check_elf(path):
    """A musl target name alone cannot establish a static runtime contract."""
    header = run("readelf", "-h", str(path)).stdout
    program = run("readelf", "-l", str(path)).stdout
    dynamic = run("readelf", "-d", str(path)).stdout
    if ("ELF64" not in header or "Advanced Micro Devices X86-64" not in header
            or "INTERP" in program or "(NEEDED)" in dynamic):
        raise RuntimeError("release must be x86-64 ELF without an interpreter or needed libraries")


def check_pe(path):
    locator = Path(os.environ["ProgramFiles(x86)"]) / "Microsoft Visual Studio/Installer/vswhere.exe"
    installation = run(str(locator), "-latest", "-products", "*", "-requires",
                       "Microsoft.VisualStudio.Component.VC.Tools.x86.x64", "-property", "installationPath").stdout.strip()
    candidates = sorted(Path(installation).glob("VC/Tools/MSVC/*/bin/Hostx64/x64/dumpbin.exe"))
    if not candidates:
        raise RuntimeError("MSVC dumpbin is required to audit release imports")
    imports = run(str(candidates[-1]), "/dependents", str(path)).stdout.lower()
    if re.search(r"(?:vcruntime|msvcp|msvcr[0-9]|ucrtbase|libpq|odbc|msodbcsql|libssl|libcrypto)[^\s]*\.dll", imports):
        raise RuntimeError("release imports a redistributable C++ or database/TLS runtime")
    headers = run(str(candidates[-1]), "/headers", str(path)).stdout
    if "8664 machine (x64)" not in headers:
        raise RuntimeError("release must be native x86-64 PE")


def pki(root, common_name="pbps-db"):
    (root / "empty").mkdir()
    for name in ("ca", "untrusted"):
        run("openssl", "req", "-x509", "-newkey", "rsa:2048", "-nodes",
            "-days", "2", "-subj", f"/CN=pbps release {name}",
            "-keyout", str(root / f"{name}.key"), "-out", str(root / f"{name}.pem"))
    run("openssl", "req", "-newkey", "rsa:2048", "-nodes", "-subj",
        f"/CN={common_name}", "-keyout", str(root / "peer.key"),
        "-out", str(root / "peer.csr"))
    (root / "extensions").write_text(
        "subjectAltName=DNS:pbps-db,DNS:localhost\nbasicConstraints=CA:FALSE\n"
        "keyUsage=digitalSignature,keyEncipherment\nextendedKeyUsage=serverAuth\n")
    run("openssl", "x509", "-req", "-in", str(root / "peer.csr"),
        "-CA", str(root / "ca.pem"), "-CAkey", str(root / "ca.key"),
        "-CAcreateserial", "-days", "2", "-extfile", str(root / "extensions"),
        "-out", str(root / "peer.pem"))


@contextmanager
def linux_engine(engine, root, network):
    name = "pbps-release-db-" + uuid.uuid4().hex
    image = IMAGES[engine]
    if engine == "postgres":
        env = ["-e", f"POSTGRES_PASSWORD={PASSWORD}"]
        command = ("chown postgres:postgres /tmp/peer.key; chmod 600 /tmp/peer.key; "
                   "exec docker-entrypoint.sh postgres -c ssl=on "
                   "-c ssl_cert_file=/tmp/peer.pem -c ssl_key_file=/tmp/peer.key")
        probe = ["pg_isready", "-h", "127.0.0.1", "-U", "postgres"]
        sql = ["psql", "-U", "postgres", "-v", "ON_ERROR_STOP=1", "-Atc"]
        version_sql = "SELECT version()"
        port = 5432
    else:
        env = ["-e", "ACCEPT_EULA=Y", "-e", f"MSSQL_SA_PASSWORD={PASSWORD}"]
        command = ("chown mssql:root /tmp/peer.key; chmod 600 /tmp/peer.key; "
                   "exec su -s /bin/bash mssql -c /opt/mssql/bin/sqlservr")
        sql = ["/opt/mssql-tools18/bin/sqlcmd", "-C", "-S", "localhost",
               "-U", "sa", "-P", PASSWORD, "-b", "-Q"]
        probe = [*sql, "SELECT 1"]
        version_sql = "SELECT @@VERSION"
        port = 1433
        (root / "mssql.conf").write_text(
            "[network]\ntlscert = /tmp/peer.pem\ntlskey = /tmp/peer.key\nforceencryption = 1\n")
    try:
        run("docker", "create", "--name", name, "--network", network,
            "--network-alias", "pbps-db", "--user", "0", "--memory", "3g",
            *env, "--entrypoint", "/bin/bash", image, "-ec", command)
        for file in ("peer.key", "peer.pem"):
            run("docker", "cp", str(root / file), f"{name}:/tmp/{file}")
        if engine == "mssql":
            run("docker", "cp", str(root / "mssql.conf"), f"{name}:/var/opt/mssql/mssql.conf")
        run("docker", "start", name)
        for _ in range(90):
            if run("docker", "exec", name, *probe, codes=range(256)).returncode == 0:
                break
            time.sleep(2)
        else:
            raise RuntimeError(f"owned {engine} fixture did not become ready")
        version = run("docker", "exec", name, *sql, version_sql).stdout.strip()
        run("docker", "exec", name, *sql, "CREATE DATABASE pbps_release")
        address = json.loads(run("docker", "inspect", name).stdout)[0]["NetworkSettings"]["Networks"][network]["IPAddress"]
        yield {"port": port, "wrong_host": address, "version": version, "image": image}
    finally:
        run("docker", "rm", "-fv", name)


class Consumer:
    def __init__(self, name, windows, stage):
        self.name, self.windows, self.stage = name, windows, stage
        self.root = "C:\\work" if windows else "/work"
        self.bin = self.path("pbps.exe" if windows else "pbps")
        self.project = self.path("project")

    def path(self, name):
        return self.root + ("\\" if self.windows else "/") + name.replace("/", "\\" if self.windows else "/")

    def exec(self, *args, env=None, codes=(0,)):
        environment = []
        for key, value in (env or {}).items():
            environment.extend(["-e", f"{key}={value}"])
        return run("docker", "exec", *environment, "-w", self.project, self.name, *args, codes=codes)

    def cli(self, *args, env=None, codes=(0,)):
        return self.exec(self.bin, "--project", self.project, "--no-input", *args,
                         env=env, codes=codes)

    def report(self, *args, env=None, codes=(0,)):
        result = self.cli(*args, "--format=json", env=env, codes=codes)
        return result.returncode, json.loads(result.stdout)

    def commit(self):
        self.exec("git", "add", "schema", "schema.ids.json", "pbps.yml")
        self.exec("git", "commit", "-qm", "Record release qualification declaration")


def connection(engine, host, port):
    if engine == "postgres":
        return f"host={host} port={port} user=postgres password={PASSWORD} dbname=pbps_release sslmode=require"
    return f"server=tcp:{host},{port};user=sa;password={PASSWORD};database=pbps_release;Encrypt=true"


def require_tls_refusal(code, report):
    details = json.dumps(report).lower()
    if (code != 1 or report.get("result") != "unanswerable"
            or not re.search(r"certificate|unknownissuer|notvalidforname|error performing tls handshake", details)):
        raise RuntimeError("peer/trust control did not produce a TLS handshake refusal: " + details.replace(PASSWORD, "[redacted]")[:5000])


def workload(consumer, engine, fixture, expected_hash):
    done = []

    def passed(name):
        if name in done or name not in CASES:
            raise RuntimeError("invalid case accounting")
        done.append(name)
        print(json.dumps({"engine": engine, "case": name, "result": "pass"}), flush=True)

    if consumer.windows:
        actual = consumer.exec("powershell.exe", "-NoProfile", "-Command",
                               f"(Get-FileHash -Algorithm SHA256 '{consumer.bin}').Hash").stdout.strip().lower()
    else:
        actual = consumer.exec("sha256sum", consumer.bin).stdout.split()[0]
    if actual != expected_hash:
        raise RuntimeError("consumer artifact differs from producer")
    passed("artifact-identity")
    env = {"PBPS_RELEASE_DB": connection(engine, "pbps-db", fixture["port"]),
           "SSL_CERT_FILE": consumer.path("ca.pem"), "SSL_CERT_DIR": consumer.path("empty")}
    project = consumer.stage / "project"
    table = "public.release_table" if engine == "postgres" else "dbo.release_table"
    (project / "pbps.yml").write_text(
        f"dialect: {engine}\nenvironments:\n  release:\n    url_env: PBPS_RELEASE_DB\n", encoding="utf-8")
    declaration = f"table: {table}\ncolumns:\n  id: {{type: int, nullable: false}}\nprimary_key: [id]\n"
    if engine == "postgres":
        declaration = declaration.replace("type: int", "type: integer")
    schema = project / "schema" / "table.yml"
    schema.write_text(declaration, encoding="utf-8")
    consumer.exec("git", "config", "--global", "--add", "safe.directory", consumer.project)
    for args in [("init", "-q"), ("config", "user.email", "release@example.invalid"),
                 ("config", "user.name", "Release qualification"), ("config", "core.autocrlf", "false")]:
        consumer.exec("git", *args)
    consumer.cli("--version")
    consumer.cli("--help")
    consumer.cli("plan", "--no-dev", "--out", consumer.path("preview.json"))
    _, preview = consumer.report("explain", "--plan", consumer.path("preview.json"))
    if preview["data"]["applyable"] is not False or preview["data"]["change_count"] != 1:
        raise RuntimeError("offline plan did not describe the declared table")
    passed("offline")
    consumer.commit()
    consumer.cli("bootstrap", "--env", "release", env=env)
    passed("trusted-bootstrap")
    for name, changed in [("wrong-peer", {"PBPS_RELEASE_DB": connection(engine, fixture["wrong_host"], fixture["port"])}),
                          ("untrusted-root", {"SSL_CERT_FILE": consumer.path("untrusted.pem")})]:
        code, error = consumer.report("verify", "--env", "release", env={**env, **changed}, codes=(1,))
        require_tls_refusal(code, error)
        passed(name)
    _, clean = consumer.report("verify", "--env", "release", env=env)
    if clean["result"] != "ok" or clean["data"]["changes"]["changes"]:
        raise RuntimeError("restored trust failed to verify the bootstrap")
    baseline_checksum = clean["data"]["live_checksum"]
    passed("restored-trust")
    schema.write_text(declaration.replace("primary_key:", "  note: {type: int, nullable: true}\nprimary_key:")
                      .replace("type: int,", "type: integer," if engine == "postgres" else "type: int,"), encoding="utf-8")
    consumer.cli("plan", "--no-dev", "--out", consumer.path("preview-change.json"))
    consumer.commit()
    consumer.cli("plan", "--env", "release", "--out", consumer.path("change.json"), env=env)
    _, plan = consumer.report("explain", "--plan", consumer.path("change.json"))
    if plan["data"]["applyable"] is not True or plan["data"]["change_count"] != 1:
        raise RuntimeError("connected plan did not contain one applyable change")
    checksum = plan["data"]["checksum"]
    if not re.fullmatch(r"[a-f0-9]{64}", checksum):
        raise RuntimeError("missing plan checksum")
    passed("connected-plan")
    _, before = consumer.report("state", "list", "--env", "release", env=env)
    refused = consumer.cli("apply", "--env", "release", "--plan", consumer.path("change.json"),
                                 "--checksum", "0" * 64, env=env, codes=(1,))
    if "checksum" not in (refused.stdout + refused.stderr).lower():
        raise RuntimeError("wrong checksum was refused for an unrelated reason")
    passed("wrong-checksum")
    _, after = consumer.report("state", "list", "--env", "release", env=env)
    if before["data"] != after["data"]:
        raise RuntimeError("refused apply wrote a ledger entry")
    _, drift = consumer.report("verify", "--env", "release", env=env)
    if (drift["result"] != "ok" or drift["data"]["changes"]["changes"]
            or drift["data"]["live_checksum"] != baseline_checksum):
        raise RuntimeError("refused apply changed the database schema")
    passed("unchanged-after-refusal")
    consumer.cli("apply", "--env", "release", "--plan", consumer.path("change.json"), "--checksum", checksum, env=env)
    passed("apply")
    _, clean = consumer.report("verify", "--env", "release", env=env)
    if clean["result"] != "ok" or clean["data"]["changes"]["changes"]:
        raise RuntimeError("applied artifact did not converge")
    if clean["data"]["live_checksum"] == baseline_checksum:
        raise RuntimeError("apply did not change the live schema")
    consumer.cli("plan", "--env", "release", "--out", consumer.path("converged.json"), env=env)
    _, converged = consumer.report("explain", "--plan", consumer.path("converged.json"))
    if converged["data"]["change_count"] != 0:
        raise RuntimeError("live schema does not match the intended declarations")
    passed("verify")
    _, ledger = consumer.report("state", "list", "--env", "release", env=env)
    if len(ledger["data"]["entries"]) != 2:
        raise RuntimeError("expected bootstrap and apply ledger entries")
    passed("ledger")
    if tuple(done) != CASES:
        raise RuntimeError("incomplete release qualification")
    return done


def interrupted(signum, _frame):
    raise SystemExit(128 + signum)


def require_complete(engines):
    if set(engines) != set(IMAGES) or any(tuple(value.get("cases", ())) != CASES for value in engines.values()):
        raise RuntimeError("both engines must complete every case exactly once and in order")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--consumer-image", required=True)
    parser.add_argument("--evidence", type=Path, required=True)
    parser.add_argument("--windows-fixture", type=Path)
    args = parser.parse_args()
    binary = args.binary.resolve(strict=True)
    windows = platform.system() == "Windows"
    if platform.system() not in ("Linux", "Windows"):
        parser.error("only native Linux and Windows are qualified")
    if windows != bool(args.windows_fixture):
        parser.error("Windows requires an owned native fixture manifest")
    if run("docker", "info", "--format", "{{.OSType}}").stdout.strip() != ("windows" if windows else "linux"):
        raise RuntimeError("consumer Docker daemon must match the native platform")
    if not windows:
        check_elf(binary)
    else:
        check_pe(binary)
    signal.signal(signal.SIGTERM, interrupted)
    signal.signal(signal.SIGINT, interrupted)
    artifact_hash = digest(binary)
    evidence = {"platform": platform.platform(), "sha256": artifact_hash,
                "consumer_image": json.loads(run("docker", "image", "inspect", args.consumer_image).stdout)[0]["Id"],
                "rustc": run("rustc", "-vV").stdout,
                "source_commit": run("git", "-C", str(Path(__file__).resolve().parents[1]), "rev-parse", "HEAD").stdout.strip(),
                "lockfile_sha256": digest(Path(__file__).resolve().parents[1] / "Cargo.lock"),
                "engines": {}}
    network = "pbps-release-" + uuid.uuid4().hex
    native = json.loads(args.windows_fixture.read_text(encoding="utf-8-sig")) if windows else None
    with tempfile.TemporaryDirectory(prefix="pbps-release-") as directory:
        root = Path(directory)
        if not windows:
            pki(root)
            run("docker", "network", "create", "--internal", network)
        try:
            for engine in IMAGES:
                stage = root / engine
                (stage / "project" / "schema").mkdir(parents=True)
                (stage / "empty").mkdir()
                shutil.copy2(binary, stage / ("pbps.exe" if windows else "pbps"))
                trust = Path(native["trust"]) if windows else root
                for file in ("ca.pem", "untrusted.pem"):
                    shutil.copy2(trust / file, stage / file)
                name = "pbps-release-client-" + uuid.uuid4().hex
                consumer = Consumer(name, windows, stage)
                try:
                    if windows:
                        command = ["--isolation=process", "--add-host", f"pbps-db:{native['gateway']}",
                                   "-v", f"{stage}:C:\\work", args.consumer_image,
                                   "cmd.exe", "/c", "ping -t 127.0.0.1 >NUL"]
                    else:
                        command = ["--network", network, "--user", f"{os.getuid()}:{os.getgid()}",
                                   "-e", "HOME=/tmp", "-v", f"{stage}:/work",
                                   args.consumer_image, "sleep", "infinity"]
                    run("docker", "run", "-d", "--name", name, *command)
                    if windows:
                        fixture = native[engine]
                        print(json.dumps({"engine": engine, "fixture": fixture}), flush=True)
                        cases = workload(consumer, engine, fixture, artifact_hash)
                        evidence["engines"][engine] = {"fixture": fixture, "cases": cases}
                    else:
                        with linux_engine(engine, root, network) as fixture:
                            print(json.dumps({"engine": engine, "fixture": fixture}), flush=True)
                            cases = workload(consumer, engine, fixture, artifact_hash)
                            evidence["engines"][engine] = {"fixture": fixture, "cases": cases}
                finally:
                    run("docker", "rm", "-fv", name)
        finally:
            if not windows:
                run("docker", "network", "rm", network)
    if digest(binary) != artifact_hash:
        raise RuntimeError("producer artifact changed during qualification")
    require_complete(evidence["engines"])
    args.evidence.write_text(json.dumps(evidence, indent=2) + "\n", encoding="utf-8")


if __name__ == "__main__":
    main()
