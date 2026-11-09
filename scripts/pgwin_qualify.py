#!/usr/bin/env python3
"""Qualify the resolver flows on the Windows runner's native PostgreSQL (#2491, W2).

Run by the `resolver-pg-windows` job; nothing here can run anywhere else, and
the pure helpers are covered by `scripts/pgwin_qualify_test.py` on every
platform. It is `scripts/localdb_qualify.py`'s counterpart for PostgreSQL and
shares its helpers: the dedicated ordinary user, the cmd wrapper that runs a
command as that user, the exact-count rule for libtest runs.

What the job does, in order, and fails on the first thing that does not hold:

1. Creates one ordinary local user (`pbpslocaldb`, the helper's name). The
   clusters and the tests belong to it, not to the runner's administrator:
   the qualification is about what an unprivileged user gets.
2. Builds the `flow_pg` test program as the administrator, into a directory
   that user can read and run. Nothing is compiled as the user.
3. As that user, creates two clusters with the PostgreSQL the hosted image
   ships (`initdb`, scram-sha-256, a fresh random password each, loopback
   only, fixed ports): the target, and a second one the server scratch kinds
   use. It asserts the major version is 17; another major fails the job, it
   does not quietly change what is qualified.
4. Runs the named flow cases as that user, each held to running and passing
   exactly once (`run_case`), and stops the clusters.

The image ships one major, so the PostgreSQL 16 half of the two-server tests
and the Docker scratch kind are not exercised here; the acceptance document
says so.
"""

import os
import re
import secrets
from pathlib import Path

import localdb_qualify as lq

# The only major the hosted image carries and this job qualifies.
MAJOR = 17
PGBIN = Path(os.environ.get("PGBIN") or r"C:\Program Files\PostgreSQL\17\bin")
DATABASE = "pbps_test"
# Fixed loopback ports, clear of the ephemeral range and of the LocalDB job's.
CLUSTERS = {"target": 55441, "scratch": 55442}
PGROOT = lq.ROOT / "pg"
# Test-only speed: no durability is asked of a cluster that lives for one job.
SERVER_OPTIONS = "-c listen_addresses=127.0.0.1 -c fsync=off -c synchronous_commit=off -c full_page_writes=off"


def connection(port, password):
    """The connection string the Linux fixtures spell, for a loopback cluster."""
    return (f"host=127.0.0.1 port={port} user=postgres password={password} "
            f"dbname={DATABASE} sslmode=disable")


def require_major(version_num):
    """Raise unless `SHOW server_version_num` is a PostgreSQL of the qualified major."""
    text = version_num.strip()
    if not re.fullmatch(r"[0-9]{6}", text):
        raise RuntimeError(f"unreadable server_version_num: {text!r}")
    if int(text) // 10000 != MAJOR:
        raise RuntimeError(f"the cluster is PostgreSQL {int(text) // 10000}, not the qualified {MAJOR}")
    return text


def pgpass(passwords):
    """A password file naming each cluster, so that no password is on a command line."""
    return "".join(f"127.0.0.1:{CLUSTERS[name]}:*:postgres:{passwords[name]}\n" for name in CLUSTERS)


def server_arguments(name):
    return f"-p {CLUSTERS[name]} {SERVER_OPTIONS}"


# Everything below needs Windows.

def step(secret, label, argv, env=None, seconds=600):
    """One command as the dedicated user; its output is printed, a failure raises."""
    code, text = lq.as_user(secret, argv, env=env, seconds=seconds)
    print(f"[{label}] exit {code}\n{text}")
    if code:
        raise RuntimeError(f"{label} failed (exit {code})")
    return text


def provision(secret, passwords):
    """Two clusters, started, each holding `pbps_test`, of the qualified major."""
    PGROOT.mkdir(parents=True, exist_ok=True)
    pgpass_file = lq.ROOT / "pgpass.conf"
    pgpass_file.write_text(pgpass(passwords), encoding="ascii")
    for name in CLUSTERS:
        data = PGROOT / name
        pwfile = lq.ROOT / f"{name}.pw"
        pwfile.write_text(passwords[name], encoding="ascii")
        step(secret, f"initdb {name}", [PGBIN / "initdb.exe", "-D", data, "-U", "postgres",
                                        "--auth=scram-sha-256", f"--pwfile={pwfile}",
                                        "-E", "UTF8", "--locale=C"])
        pwfile.unlink()
        step(secret, f"start {name}", [PGBIN / "pg_ctl.exe", "start", "-D", data, "-w", "-t", "120",
                                       "-l", PGROOT / f"{name}.log", "-o", server_arguments(name)])
    env = {"PGPASSFILE": str(pgpass_file)}
    for name, port in CLUSTERS.items():
        psql = [PGBIN / "psql.exe", "-h", "127.0.0.1", "-p", str(port), "-U", "postgres", "-w", "-d", "postgres"]
        step(secret, f"create {DATABASE} on {name}", [*psql, "-c", f"CREATE DATABASE {DATABASE}"], env=env)
        text = step(secret, f"version of {name}", [*psql, "-Atc", "SHOW server_version_num"], env=env)
        print(f"{name}: server_version_num {require_major(text.strip().splitlines()[-1])}")


def stop(secret):
    for name in CLUSTERS:
        data = PGROOT / name
        if data.exists():
            lq.as_user(secret, [PGBIN / "pg_ctl.exe", "stop", "-D", data, "-m", "immediate", "-w", "-t", "60"])


def main():
    results = []
    secret = lq.password()
    passwords = {name: secrets.token_hex(16) for name in CLUSTERS}
    for password in passwords.values():
        print(f"::add-mask::{password}", flush=True)
    lq.create_user(secret)
    flow = lq.executable(lq.cargo_json(["test", "--no-run", "-p", "pbps-cli", "--test", "flow_pg"]),
                         "pbps-cli", "test", "flow_pg")
    try:
        provision(secret, passwords)
        env = {
            "PBPS_TEST_PG_DB": connection(CLUSTERS["target"], passwords["target"]),
            "PBPS_TEST_PG_SCRATCH_DB": connection(CLUSTERS["scratch"], passwords["scratch"]),
            # A key file's owner-only permission cannot be proved on Windows, so
            # the fixtures name this variable instead (flow_pg.rs KEY_ENV).
            "PBPS_TEST_FINGERPRINT_KEY": lq.fingerprint_key(),
            # A debug build on Windows has larger frames than libtest's default
            # thread stack (see localdb_qualify.py).
            "RUST_MIN_STACK": str(8 * 1024 * 1024),
        }

        def run_case(binary, test):
            code, text = lq.as_user(secret, [binary, "--ignored", "--exact", test, "--test-threads=1",
                                             "--format=pretty", "--color=never"], env=env)
            print(text)
            lq.require_case(text, test)
            if code:
                raise RuntimeError(f"{test} exited {code}")
            results.append(("pgwin", test, 1))

        run_case(flow, "pgwin2491::pg_rehearsal_converges_on_every_native_scratch")

        if not results or sum(count for *_, count in results) == 0:
            raise RuntimeError("no qualification case ran")
        for label, name, count in results:
            print(f"{count:4d}  {label}  {name}")
        print("PostgreSQL on Windows qualification: all cases passed")
    finally:
        stop(secret)


if __name__ == "__main__":
    main()
