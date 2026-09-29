#!/usr/bin/env python3
"""Qualify the dedicated scratch-server profile against actual engines.

Builds disposable deployments on a native Linux host: a TLS target, one
supplied scratch server contained exactly as its named profile requires, an
alias endpoint whose container *is* the target's, and one whose container is
on a bridge network. Requires root, because reading another service's process,
namespace and cgroup facts does, and a container runtime on the Docker API.

The supplied servers are started by this script, not by pbps: that is the
point. pbps reads the daemon's record of them and measures the kernel, the way
it would for a container an operator started from the documented recipe. No
host path, volume or runtime socket is mounted into any of them.
"""

import argparse
from fixture_diagnostics import report
import http.client
import json
import os
from pathlib import Path
import socket
import shutil
import subprocess
import sys
import time
import uuid

IMAGES = {
    "pg": "postgres@sha256:4ef4dbc939d61acea57712655ddb4b4ab27419c913f94cca0cd57cb3ea3c2280",
    "mssql": "mcr.microsoft.com/mssql/server@sha256:4bab24f36c1ecd48e85f7d37df26e6bf301641d84c3fe652f9a0dcc947d512e1",
}
PG_IMAGES = {
    16: "postgres@sha256:485935f94cc7165afa896978809c37b592dc07f0a37d2c8f645f12412d0212c8",
    18: IMAGES["pg"],
}
PG_PROFILES = {16: "linux-dedicated-pg16-v1", 18: "linux-dedicated-v1"}
PASSWORD = "Pbps!DedicatedFixture12345"
MARKER = "pbps_marker_preexisting"
ENGINE_UID = {"pg": 999, "mssql": 10001}
EXECUTABLE = {"pg": "postgres", "mssql": "sqlservr"}
QUIET = {"stdout": subprocess.DEVNULL, "stderr": subprocess.DEVNULL}
TESTS = [
    "admission_recovery::failed_admission_names_every_unconfirmed_forwarder",
    "admission_recovery::a_cancelled_step_leaves_its_admin_session_to_cleanup",
    "forwarder_mqueue::every_session_qualifies_its_forwarders_mqueue_before_returning",
    "forwarder_mqueue::a_foreign_forwarder_mqueue_discards_each_live_view_permanently",
    "host_files::host_file_loss_refuses_admission_and_discards_live_analysis",
    "uts::kernel_name_loss_refuses_admission_and_discards_each_live_view",
    "pseudo::foreign_pseudo_roots_refuse_admission_and_discard_live_analysis",
    "a_supported_dedicated_server_compiles_declarations_and_removes_only_its_own_resources",
    "pg16_storage::the_supplied_storage_layout_admits_its_observed_major_and_survives_live_checks",
    "guard_limits::guard_restoration_distinguishes_process_exit_from_live_cleanup_failure",
    "guard_limits::every_forwarder_guard_requires_effective_descriptor_evidence",
    # PostgreSQL analysis-scope qualification (#610); no-ops on SQL Server (#611).
    "a_run_qualifies_its_analysis_scope_against_the_target",
    "a_server_inside_the_target_instance_is_refused_before_any_scratch_resource",
    # Desired-binding resolution through a qualified run (#613); SQL Server
    # is refused by name until its adapter exists (#619, #620).
    "bindings::a_qualified_run_resolves_desired_bindings_against_the_target",
    # Runs with the exposed control started; see below.
    "an_unimplemented_profile_or_an_exposed_runtime_is_refused_by_name",
    "a_session_this_run_did_not_open_invalidates_it_even_after_it_closed",
    "a_session_present_at_admission_is_refused_rather_than_counted",
    "a_removed_statistics_row_cannot_pay_for_an_intruding_session",
    # Runs with a deliberate intruder in the engine's namespaces; see below.
    "a_process_the_engine_did_not_start_refuses_its_namespaces",
    # Runs with a container joined to the engine's network namespace.
    "a_container_joined_to_the_engines_network_refuses_the_run",
    "replacing_or_dropping_the_target_binding_discards_the_run",
    # Stops the supplied server under a live run, so it is last.
    "an_unconfirmed_cleanup_reports_only_the_run_owned_names",
]
# New producer cases live beside the unchanged supplied-server tests.
PRODUCER_TESTS = [
    "resolver::server::qualified_evidence_tests::the_supplied_producer_seals_the_overload_and_default_from_one_fresh_read",
    "resolver::server::qualified_evidence_tests::the_supplied_empty_target_producer_orders_table_routines_and_expressions_before_sealing",
    "resolver::server::qualified_evidence_tests::the_supplied_replaced_routine_rebuilds_cross_kind_dependents_in_the_final_plan",
    "resolver::server::qualified_evidence_tests::the_supplied_recorded_table_and_column_uids_survive_rename_with_dependent_rebuilds",
    "resolver::server::qualified_evidence_tests::the_supplied_rebuilt_routine_with_explicit_public_execution_matches_the_post_ddl_acl",
]
DOCKER_SOCKET = "/var/run/docker.sock"

# The recipe an operator follows. Everything writable is a tmpfs the runtime
# creates, the image root is read-only, the network namespace is empty and
# every privilege the engine does not need is gone before it starts.
RECIPE = [
    "--network", "none", "--ipc", "private", "--read-only",
    "--hostname", "pbps-resolver", "--dns", "127.0.0.1",
    "--dns-search", ".", "--dns-option", "ndots:0",
    "--tmpfs", "/tmp:rw,nosuid,nodev,noexec,size=67108864,mode=1777",
    # /run and /var/tmp forced to noexec: Podman auto-mounts them rw without
    # noexec, and the profile requires no executable private storage.
    "--tmpfs", "/run:rw,nosuid,nodev,noexec,size=67108864,mode=755",
    "--tmpfs", "/var/tmp:rw,nosuid,nodev,noexec,size=67108864,mode=1777",
    "--security-opt", "no-new-privileges",
    "--memory", "3g", "--memory-swap", "3g", "--cpus", "2", "--pids-limit", "512",
]
# SQL Server's own telemetry client (`SQLServerCEIP`) logs in over loopback a
# few minutes after the engine starts — measured on 17.0: five and a half
# minutes, as `NT AUTHORITY\SYSTEM` from 127.0.0.1 — and to a census of the
# engine's namespace that is a session this run did not open. A run that was
# still open at that moment was refused as not exclusive, which no test here
# lived long enough to meet until one hashed the engine's packages (#611). The
# operator of a dedicated scratch server turns customer feedback off, as this
# does; with it off no such session appears (measured, Developer edition).
# The file has to be written at start: its directory is the tmpfs below.
MSSQL_BOOT = """
printf '[telemetry]\\ncustomerfeedback = false\\n' > /var/opt/mssql/mssql.conf
exec /opt/mssql/bin/launch_sqlservr.sh /opt/mssql/bin/sqlservr
"""
STORAGE = {
    "pg": "/var/lib/postgresql:rw,nosuid,nodev,noexec,size=268435456,uid=999,gid=999,mode=700",
    "mssql": "/var/opt/mssql:rw,nosuid,nodev,noexec,size=1073741824,uid=10001,gid=0,mode=700",
}

# The runtime prepares the storage for uid/gid 999. Both initdb and the engine
# can therefore start without root or any capability; there is no privileged
# ownership shell to qualify or leave behind (DECISIONS 531).
POSTGRES_BOOT = """
set -e
umask 077
printf '%s' "$PBPS_FIXTURE_PASSWORD" > /var/lib/postgresql/pw
/usr/lib/postgresql/18/bin/initdb \
  -D /var/lib/postgresql/run-data --auth-local=reject --auth-host=scram-sha-256 \
  --pwfile=/var/lib/postgresql/pw >/dev/null
rm /var/lib/postgresql/pw
exec /usr/lib/postgresql/18/bin/postgres -D /var/lib/postgresql/run-data \
  -c listen_addresses=127.0.0.1 -c unix_socket_directories=
"""


# One child tmpfs replaces the PG16 image's declared VOLUME (DEC-1302.1).
# Keep the existing PG18 parent recipe unchanged instead of adding a mount.
PG_STORAGE = {
    16: "/var/lib/postgresql/data:rw,nosuid,nodev,noexec,size=268435456,uid=999,gid=999,mode=700",
    18: STORAGE["pg"],
}
PG_BOOT = {
    16: """
set -e
umask 077
printf '%s' "$PBPS_FIXTURE_PASSWORD" > /var/lib/postgresql/data/pw
/usr/lib/postgresql/16/bin/initdb \
  -D /var/lib/postgresql/data/run-data --auth-local=reject --auth-host=scram-sha-256 \
  --pwfile=/var/lib/postgresql/data/pw >/dev/null
rm /var/lib/postgresql/data/pw
exec /usr/lib/postgresql/16/bin/postgres -D /var/lib/postgresql/data/run-data \
  -c listen_addresses=127.0.0.1 -c unix_socket_directories=
""",
    18: POSTGRES_BOOT,
}


def fixture_image(engine, pg_major=18):
    return PG_IMAGES[pg_major] if engine == "pg" else IMAGES[engine]


def run(*args, **kwargs):
    if args[0] == "docker":
        args = ("docker", "--host", "unix://" + DOCKER_SOCKET, *args[1:])
    kwargs.setdefault("check", True)
    kwargs.setdefault("text", True)
    return subprocess.run(args, **kwargs)


def certificates(root):
    run("openssl", "req", "-x509", "-newkey", "rsa:2048", "-nodes", "-days", "2",
        "-subj", "/CN=pbps dedicated test CA", "-keyout", str(root / "ca.key"),
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


def service_pid(container, executable):
    rows = [line.split() for line in run(
        "docker", "top", container, "-eo", "pid,ppid,comm",
        stdout=subprocess.PIPE).stdout.splitlines()[1:]]
    processes = {row[0] for row in rows if row[2] == executable}
    roots = [row[0] for row in rows if row[0] in processes and row[1] not in processes]
    if len(roots) != 1:
        raise RuntimeError(f"ambiguous service process in {container}")
    return roots[0]


def await_engine(container, engine):
    if engine == "pg":
        probe = ["pg_isready", "-h", "127.0.0.1"]
    else:
        probe = ["/opt/mssql-tools18/bin/sqlcmd", "-C", "-S", "127.0.0.1", "-U", "sa",
                 "-P", PASSWORD, "-Q", "SELECT 1"]
    for _ in range(90):
        if run("docker", "exec", container, *probe, check=False, **QUIET).returncode == 0:
            return
        # An engine that has exited will not become ready. Waiting out the
        # loop for one only delays the report by three minutes (#958).
        if not running(container):
            break
        time.sleep(2)
    # Say why. The cleanup below removes the container, so whatever is not
    # printed here is the last anyone reading CI ever sees of it. Through the
    # shared reporter rather than inline: this used to interpolate the log into
    # the message, and on the occurrence that filed #724 that log was empty —
    # which printed as nothing at all, indistinguishable from a fixture that
    # does not print logs.
    report(run, container)
    raise RuntimeError(f"owned fixture {container} did not become ready")


def running(container):
    done = run("docker", "inspect", "--format", "{{.State.Running}}", container,
               check=False, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL)
    # Unreadable is not "exited": keep waiting, and let the loop's own limit
    # decide.
    return done.returncode != 0 or done.stdout.strip() == "true"


def crashed_starting(container, engine):
    """Whether SQL Server core-dumped before it ever answered (#958, DEC-958.1).

    The shape measured in CI, five merge groups out of five: the engine dumps
    about two seconds after `docker start`, before any test has touched it, and
    `paldumper` names the dump in the container's log. Nothing the code under
    test does can reach an engine at that point, so this is the one failure
    worth a second start. Any other failure stays a failure.
    """
    if engine != "mssql" or running(container):
        return False
    done = run("docker", "logs", container, check=False, stdout=subprocess.PIPE,
               stderr=subprocess.STDOUT)
    return done.returncode == 0 and "Dump already generated: /var/opt/mssql/log/core.sqlservr" in done.stdout


def ready(container, engine, start):
    """Wait for `container`'s engine, starting it once more if SQL Server
    crashed while starting. `start` recreates the container under the same
    name; a second crash fails the run, since a crash that repeats is no
    longer the transient one this retries."""
    try:
        await_engine(container, engine)
    except RuntimeError:
        if not crashed_starting(container, engine):
            raise
        print(f"{container}: SQL Server core-dumped while starting; starting it once more "
              "(#958)", flush=True)
        run("docker", "rm", "--force", "--volumes", container, **QUIET)
        start()
        await_engine(container, engine)


def start_dedicated(engine, name, owned, network=None, empty_runtime_files=False, pg_major=18):
    """One supplied server, contained as its explicit measured profile requires."""
    owned.append(name)
    recipe = list(RECIPE)
    if network is not None:
        recipe[recipe.index("none")] = network
    storage = PG_STORAGE[pg_major] if engine == "pg" else STORAGE[engine]
    image = fixture_image(engine, pg_major)
    common = ["--name", name, "--label", "io.pbps.resolver.fixture=dedicated-server",
              "--pull", "never", *recipe, "--tmpfs", storage]
    if engine == "pg":
        run("docker", "create", *common, "--user", "999:999", "--cap-drop", "ALL",
            "-e", f"PBPS_FIXTURE_PASSWORD={PASSWORD}",
            "--entrypoint", "/bin/bash", image, "-ec", PG_BOOT[pg_major], **QUIET)
    else:
        # sqlservr carries cap_net_bind_service as a file capability, so an
        # empty bounding set makes its exec fail outright. The profile's
        # capability ceiling is exactly that one bit.
        run("docker", "create", *common, "--user", str(ENGINE_UID[engine]),
            "--cap-drop", "ALL", "--cap-add", "NET_BIND_SERVICE",
            "-e", "ACCEPT_EULA=Y", "-e", f"MSSQL_SA_PASSWORD={PASSWORD}",
            "-e", "MSSQL_MEMORY_LIMIT_MB=1024",
            "--entrypoint", "/bin/bash", image, "-ec", MSSQL_BOOT, **QUIET)
    if empty_runtime_files:
        # Docker's API-only NetworkDisabled layout leaves all three runtime
        # files empty while the kernel UTS names remain observable (#804).
        # Recreate only this unstarted, labelled fixture with that one change.
        record = json.loads(run("docker", "inspect", name, stdout=subprocess.PIPE).stdout)[0]
        assert record["Name"] == "/" + name and not record["State"]["Running"]
        assert record["Config"]["Labels"]["io.pbps.resolver.fixture"] == "dedicated-server"
        body = dict(record["Config"], HostConfig=record["HostConfig"], NetworkDisabled=True)
        run("docker", "rm", record["Id"], **QUIET)
        connection = http.client.HTTPConnection("localhost", timeout=15)
        connection.sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        connection.sock.settimeout(15)
        try:
            connection.sock.connect(DOCKER_SOCKET)
            connection.request("POST", "/v1.47/containers/create?name=" + name,
                               json.dumps(body), {"Content-Type": "application/json"})
            response = connection.getresponse()
            data = response.read()
            if response.status != 201:
                raise RuntimeError(f"empty-files fixture creation failed: {response.status} {data!r}")
        finally:
            connection.close()
    started(name)
    if empty_runtime_files:
        for path in ("/etc/hostname", "/etc/hosts", "/etc/resolv.conf"):
            assert run("docker", "exec", name, "/bin/cat", path, stdout=subprocess.PIPE).stdout == ""


def started(name):
    """Start a fixture container, and report it if it will not start.

    `run`'s default would raise straight past the caller's cleanup, which
    removes the container, so an engine that refused to start at all printed
    neither state nor log — the case this whole reporter exists for (#724).
    """
    if run("docker", "start", name, check=False, **QUIET).returncode:
        report(run, name)
        raise RuntimeError(f"owned fixture {name} did not start")


def start_target(engine, name, root, owned, pg_major=18):
    owned.append(name)
    if engine == "pg":
        environment = ["-e", f"POSTGRES_PASSWORD={PASSWORD}"]
        boot = ("chown postgres:postgres /tmp/peer.key; chmod 600 /tmp/peer.key; "
                "exec docker-entrypoint.sh postgres -c listen_addresses=127.0.0.1 "
                "-c ssl=on -c ssl_cert_file=/tmp/peer.pem -c ssl_key_file=/tmp/peer.key")
    else:
        environment = ["-e", "ACCEPT_EULA=Y", "-e", f"MSSQL_SA_PASSWORD={PASSWORD}",
                       "-e", "MSSQL_MEMORY_LIMIT_MB=1024"]
        boot = ("chown mssql:root /tmp/peer.key; chmod 600 /tmp/peer.key; "
                "exec su -s /bin/bash mssql -c /opt/mssql/bin/sqlservr")
        (root / "mssql.conf").write_text(
            "[network]\nipaddress=127.0.0.1\ntlscert=/tmp/peer.pem\n"
            "tlskey=/tmp/peer.key\nforceencryption=0\n")
    run("docker", "create", "--name", name, "--pull", "never", "--network", "host",
        "--user", "0", "--memory", "3g", "--cpus", "2", "--pids-limit", "512",
        *environment, "--entrypoint", "/bin/bash", fixture_image(engine, pg_major), "-ec", boot, **QUIET)
    for leaf in ("peer.key", "peer.pem"):
        run("docker", "cp", str(root / leaf), name + ":/tmp/" + leaf, **QUIET)
    if engine == "mssql":
        run("docker", "cp", str(root / "mssql.conf"),
            name + ":/var/opt/mssql/mssql.conf", **QUIET)
    started(name)


def describe(container):
    """The daemon's record and the kernel's tables, as the profile reads them.

    Printed once at startup on purpose: the unit tests pin the measured
    layouts of both runtimes, and this is where a new runtime version's
    layout is read from.
    """
    print(f"--- {container}: what the profile reads ---", flush=True)
    version = run("docker", "version", "--format", "{{.Server.Version}}",
                  stdout=subprocess.PIPE, check=False).stdout.strip()
    print(f"docker server {version}", flush=True)
    record = run("docker", "inspect", container, stdout=subprocess.PIPE, check=False).stdout
    try:
        record = json.loads(record)[0]
        host = record["HostConfig"]
        print("== HostConfig ==", json.dumps({key: host.get(key) for key in (
            "Privileged", "NetworkMode", "ReadonlyRootfs", "PidMode", "IpcMode", "UTSMode",
            "UsernsMode", "CgroupnsMode", "CapDrop", "CapAdd", "SecurityOpt", "Memory",
            "MemorySwap", "NanoCpus", "PidsLimit", "Tmpfs", "Binds", "RestartPolicy")}),
            flush=True)
        print("== Mounts ==", json.dumps(record.get("Mounts")), flush=True)
    except (ValueError, KeyError, IndexError):
        print("== inspect == unreadable", flush=True)
    for label, command in [
        ("mountinfo", ["cat", "/proc/1/mountinfo"]),
        ("status", ["cat", "/proc/1/status"]),
        ("cgroup", ["cat", "/proc/1/cgroup"]),
        ("net/dev", ["cat", "/proc/net/dev"]),
        ("net/tcp", ["cat", "/proc/net/tcp"]),
        ("net/unix", ["cat", "/proc/net/unix"]),
    ]:
        output = run("docker", "exec", "--user", "0", container, *command,
                     stdout=subprocess.PIPE, stderr=subprocess.STDOUT, check=False).stdout
        print(f"== {label} ==\n{output}", end="", flush=True)


def endpoint(container, pg_major=18):
    profile = PG_PROFILES[pg_major] if ENGINE == "pg" else "linux-dedicated-v1"
    return (f"profile={profile} container={container} daemon={DOCKER_SOCKET} "
            f"user={'postgres' if ENGINE == 'pg' else 'sa'} password={PASSWORD}")


def statement(container, engine, sql, database=None):
    if engine == "pg":
        run("docker", "exec", "-e", f"PGPASSWORD={PASSWORD}", container, "psql",
            "-h", "127.0.0.1", "-U", "postgres", "-v", "ON_ERROR_STOP=1",
            "-d", database or "postgres", "-c", sql, **QUIET)
    else:
        run("docker", "exec", container, "/opt/mssql-tools18/bin/sqlcmd", "-C", "-S",
            "127.0.0.1", "-U", "sa", "-P", PASSWORD, "-d", database or "master", "-b",
            "-Q", sql, **QUIET)


ENGINE = None


def fixture(args, binary, root, owned):
    global ENGINE
    ENGINE = engine = args.engine
    pg_major = args.pg_major
    certificates(root)
    unique = uuid.uuid4().hex[:12]

    target = f"pbps-dedicated-target-{unique}"
    start_target(engine, target, root, owned, pg_major=pg_major)
    supplied = f"pbps-dedicated-server-{unique}"
    start_dedicated(engine, supplied, owned, pg_major=pg_major)
    ready(target, engine, lambda: start_target(engine, target, root, owned, pg_major=pg_major))
    ready(supplied, engine, lambda: start_dedicated(engine, supplied, owned, pg_major=pg_major))
    describe(supplied)
    # A pre-existing database no run may touch, and the counter's baseline.
    statement(supplied, engine, f"CREATE DATABASE {MARKER}")
    exposed = f"pbps-dedicated-exposed-{unique}"
    joined = f"pbps-dedicated-joined-{unique}"

    if engine == "pg":
        primary = (f"host=localhost port=5432 user=postgres password={PASSWORD} "
                   "dbname=postgres sslmode=require")
    else:
        primary = (f"Server=localhost,1433;User Id=sa;Password={PASSWORD};Database=master;"
                   # The issuer, not the leaf: rustls builds a chain to a
                   # trust anchor, and handing it the peer's own certificate
                   # leaves that chain unrooted — "UnknownIssuer".
                   f"Encrypt=true;TrustServerCertificateCA={root / 'ca.pem'}")

    environment = dict(
        PBPS_SERVER_FIXTURE="1",
        PBPS_SERVER_DRIVER=engine,
        PBPS_SERVER_PG_MAJOR=str(pg_major),
        PBPS_NATIVE_CONNECTION=primary,
        PBPS_NATIVE_SERVICE_PID=service_pid(target, EXECUTABLE[engine]),
        PBPS_SERVER_ENDPOINT=endpoint(supplied, pg_major),
        PBPS_SERVER_ALIAS_ENDPOINT=endpoint(target, pg_major),
        PBPS_SERVER_EXPOSED_ENDPOINT=endpoint(exposed, pg_major),
        PBPS_SERVER_MARKER_DATABASE=MARKER,
        SSL_CERT_FILE=str(root / "ca.pem"),
        SSL_CERT_DIR=str(root / "empty-ca"),
        PATH="/pbps-no-external-tools",
    )
    exposing = "an_unimplemented_profile_or_an_exposed_runtime_is_refused_by_name"
    intruding = "a_process_the_engine_did_not_start_refuses_its_namespaces"
    joining = "a_container_joined_to_the_engines_network_refuses_the_run"
    uts = "uts::kernel_name_loss_refuses_admission_and_discards_each_live_view"
    selected_tests = [test for test in TESTS if engine == "pg" or test !=
                      "pg16_storage::the_supplied_storage_layout_admits_its_observed_major_and_survives_live_checks"]
    cases = [(test, empty) for test in selected_tests for empty in ([False, True] if test == uts else [False])]
    if engine == "pg":
        cases.extend((test, False) for test in PRODUCER_TESTS)
    if args.producer_only:
        cases = [(test, False) for test in PRODUCER_TESTS]
    # The legacy cleanup case stops this same supplied engine. Keep it last
    # so every producer can still acquire its qualified administrative view.
    terminal_cleanup = "an_unconfirmed_cleanup_reports_only_the_run_owned_names"
    cases = [case for case in cases if case[0] != terminal_cleanup] + [
        case for case in cases if case[0] == terminal_cleanup
    ]
    for test, empty in cases:
        empty_server = f"pbps-dedicated-empty-{unique}"
        if empty:
            start_dedicated(engine, empty_server, owned, empty_runtime_files=True, pg_major=pg_major)
            ready(empty_server, engine, lambda: start_dedicated(
                engine, empty_server, owned, empty_runtime_files=True, pg_major=pg_major))
            statement(empty_server, engine, f"CREATE DATABASE {MARKER}")
        # The exposed control is a second supplied server whose runtime does
        # not give it a private network; everything else about it qualifies.
        # Started only for its own test and removed after it, so that only
        # one extra engine's startup is ever in flight.
        if test == exposing:
            start_dedicated(engine, exposed, owned, network="bridge", pg_major=pg_major)
            ready(exposed, engine, lambda: start_dedicated(engine, exposed, owned,
                                                           network="bridge", pg_major=pg_major))
        # One test needs a process the engine never started, sharing its
        # namespaces: `docker exec` joins them without becoming a descendant,
        # which is exactly the shape a subtree walk cannot see. Root, so it
        # also carries privileges the profile says the runtime removed.
        if test == intruding:
            run("docker", "exec", "-d", "--user", "0", supplied,
                "/bin/sleep", "120", **QUIET)
        # And one needs a container in the engine's network namespace that
        # is in none of its process listings.
        if test == joining:
            owned.append(joined)
            run("docker", "run", "-d", "--name", joined, "--pull", "never",
                "--network", "container:" + supplied, "--entrypoint", "/bin/sleep",
                fixture_image(engine, pg_major), "120", **QUIET)
        command = [binary, "--ignored", "--exact",
                   f"resolver::server::live_tests::{test}", "--nocapture"]
        if test in PRODUCER_TESTS:
            command[3] = test
        selected = dict(os.environ, **environment)
        if empty:
            selected["PBPS_SERVER_ENDPOINT"] = endpoint(empty_server, pg_major)
        selected["PBPS_SERVER_EMPTY_RUNTIME_FILES"] = "1" if empty else "0"
        if test.startswith("guard_limits::"):
            # Only this test's observer view hides an owned guard's limits.
            command = ["/usr/bin/unshare", "--mount", "--propagation", "private", "--", *command]
            selected["PBPS_LIMITS_PRIVATE_PROC_FIXTURE"] = "1"
            selected["PBPS_LIMITS_PARENT_MOUNT_NAMESPACE"] = os.readlink("/proc/self/ns/mnt")
        if test.startswith("admission_recovery::"):
            # Socket unavailability is confined to this observer's mount view.
            command = ["/usr/bin/unshare", "--mount", "--propagation", "private", "--", *command]
            selected["PBPS_ADMISSION_RECOVERY_FIXTURE"] = "1"
            selected["PBPS_ADMISSION_RECOVERY_DOCKER"] = str(Path(shutil.which("docker")).resolve())
        result = run(*command, env=selected, stdout=subprocess.PIPE,
                     stderr=subprocess.STDOUT, check=False)
        if test == intruding:
            run("docker", "exec", "--user", "0", supplied,
                "/usr/bin/pkill", "-f", "sleep 120", check=False, **QUIET)
        if test == joining:
            run("docker", "rm", "--force", joined, check=False, **QUIET)
        if test == exposing:
            run("docker", "rm", "--force", exposed, check=False, **QUIET)
        print(result.stdout, end="", flush=True)
        if result.returncode or "test result: ok. 1 passed" not in result.stdout:
            describe(empty_server if empty else supplied)
            print(run("docker", "ps", "-a", "--filter", "name=pbps-", stdout=subprocess.PIPE,
                      check=False).stdout, flush=True)
            raise RuntimeError(f"dedicated-server fixture failed: {test}")
        if empty:
            run("docker", "rm", "--force", "--volumes", empty_server, **QUIET)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("engine", choices=["pg", "mssql"])
    parser.add_argument("--test-binary", required=True,
                        help="the pbps-cli unit test binary carrying the live tests")
    parser.add_argument("--socket", default="/var/run/docker.sock")
    parser.add_argument("--pg-major", type=int, choices=[16, 18], default=18,
                        help="the pinned PostgreSQL version and supplied layout (default: 18)")
    parser.add_argument("--producer-only", action="store_true",
                        help="run only the focused #1274 supplied PostgreSQL producer case")
    args = parser.parse_args()
    if args.engine != "pg" and args.pg_major != 18:
        parser.error("--pg-major 16 requires the PostgreSQL engine")
    if args.producer_only and args.engine != "pg":
        parser.error("--producer-only requires pg")
    global DOCKER_SOCKET
    DOCKER_SOCKET = args.socket
    if os.geteuid() != 0:
        sys.exit("the dedicated-server fixture must run as root")
    binary = str(Path(args.test_binary).resolve())
    root = Path(f"/tmp/pbps-dedicated-{uuid.uuid4().hex[:8]}")
    root.mkdir(mode=0o700)
    owned = []
    try:
        fixture(args, binary, root, owned)
    finally:
        for resource in owned:
            run("docker", "rm", "--force", "--volumes", resource, stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL, check=False)
        subprocess.run(["rm", "-rf", str(root)], check=False)


if __name__ == "__main__":
    main()
