#!/usr/bin/env python3
"""Qualify the resolver flows on native PostgreSQL on Windows (#2491, W2).

Run by the `resolver-pg-windows` job; nothing here can run anywhere else, and
the pure helpers are covered by `scripts/pgwin_qualify_test.py` on every
platform. It is `scripts/localdb_qualify.py`'s counterpart for PostgreSQL and
shares its helpers: the dedicated ordinary user, the cmd wrapper that runs a
command as that user, the exact-count rule for libtest runs.

The resolver supports PostgreSQL 16 and 18 and refuses any other major (a
cluster of 17, the hosted image's own, fails every case at its first plan with
"not one this release can read (Version)"). So the job does not use the
image's PostgreSQL: it installs the two majors the Linux jobs qualify, from
EnterpriseDB's official Windows binary archives, pinned by exact URL and
SHA-256 as the LocalDB installer is. Nothing is installed in the Windows sense:
the archives are extracted and their programs run as the dedicated user.

What the job does, in order, and fails on the first thing that does not hold:

1. Downloads each archive, or takes the cached one, and checks its SHA-256. A
   different or missing file is a failure, not a skip.
2. Creates one ordinary local user (`pbpslocaldb`, the helper's name). The
   clusters and the tests belong to it, not to the runner's administrator:
   the qualification is about what an unprivileged user gets.
3. Builds the `flow_pg` test program as the administrator, into a directory
   that user can read and run. Nothing is compiled as the user.
4. As that user, creates three clusters with `initdb`: the target and a second
   cluster for the server scratch kinds, both PostgreSQL 18.6, and the older
   server of the two-server tests, PostgreSQL 16.15. Each is created with
   trust on its loopback, given a fresh random password from a temporary SQL
   file (never on a command line), and then closed to scram-sha-256 only.
   Nothing runs against a cluster until its `server_version_num` equals the
   pinned build and a connection with the real password has succeeded and one
   without it has been refused.
5. Runs the named flow cases as that user, each held to running and passing
   exactly once (`run_case`), and stops the clusters.

The Docker scratch kind is not exercised here: hosted Windows runs Windows
containers only. The acceptance document says so.
"""

import hashlib
import secrets
import urllib.request
import zipfile

import localdb_qualify as lq

# EnterpriseDB's Windows binary archives, one per qualified major. The build
# numbers are the Linux jobs' pins (16.15 and 18.6); `-2` is EDB's rebuild.
PINNED = {
    16: {
        "url": "https://get.enterprisedb.com/postgresql/postgresql-16.15-2-windows-x64-binaries.zip",
        "sha256": "840b9d265f6ab6c0a971c1d8e9096de564950d38dc2a5ccd98c8820179ecf115",
        "version_num": "160015",
    },
    18: {
        "url": "https://get.enterprisedb.com/postgresql/postgresql-18.6-2-windows-x64-binaries.zip",
        "sha256": "41bb1496f60666d3745ae8a855753b877d14fa9e34667f266867635b790fc323",
        "version_num": "180006",
    },
}
DATABASE = "pbps_test"
# Cluster name -> (major, fixed loopback port). The names are the fixtures'
# environment variables: PBPS_TEST_PG_DB, PBPS_TEST_PG_SCRATCH_DB and
# PBPS_TEST_PG_OLD_DB (the older server of the two-server tests).
CLUSTERS = {"target": (18, 55441), "scratch": (18, 55442), "old": (16, 55443)}
DOWNLOADS = lq.ROOT / "downloads"
INSTALL = lq.ROOT / "edb"
DATA = lq.ROOT / "pgdata"
# Test-only speed: no durability is asked of a cluster that lives for one job.
SERVER_OPTIONS = "-c listen_addresses=127.0.0.1 -c fsync=off -c synchronous_commit=off -c full_page_writes=off"
SCRAM_HBA = """host all all 127.0.0.1/32 scram-sha-256
host all all ::1/128 scram-sha-256
"""


def archive(major):
    return DOWNLOADS / PINNED[major]["url"].rsplit("/", 1)[1]


def pgbin(major, program):
    return INSTALL / f"pg{major}" / "pgsql" / "bin" / program


def connection(port, password):
    """The connection string the Linux fixtures spell, for a loopback cluster."""
    return (f"host=127.0.0.1 port={port} user=postgres password={password} "
            f"dbname={DATABASE} sslmode=disable")


def sha256_of(path):
    digest = hashlib.sha256()
    with open(path, "rb") as stream:
        for block in iter(lambda: stream.read(1 << 20), b""):
            digest.update(block)
    return digest.hexdigest()


def require_build(version_num, major):
    """Raise unless `SHOW server_version_num` is exactly the pinned build of `major`."""
    text = version_num.strip()
    if text != PINNED[major]["version_num"]:
        raise RuntimeError(f"the cluster reports server_version_num {text!r}, not the pinned "
                           f"{PINNED[major]['version_num']} of PostgreSQL {major}")
    return text


def server_arguments(port):
    return f"-p {port} {SERVER_OPTIONS}"


def bootstrap_sql(password):
    """What a fresh cluster needs once, run over its own trust-only bootstrap."""
    return f"ALTER ROLE postgres PASSWORD '{password}';\nCREATE DATABASE {DATABASE};\n"


def psql_argv(major, port, database="postgres"):
    return [pgbin(major, "psql.exe"), "-h", "127.0.0.1", "-p", str(port), "-U", "postgres", "-w", "-d", database]


# Everything below needs Windows.

def say(*parts):
    print("+", *parts, flush=True)


def fetch(major):
    """The pinned archive of `major`, downloaded unless the cached file already has the pinned hash."""
    pinned, path = PINNED[major], archive(major)
    DOWNLOADS.mkdir(parents=True, exist_ok=True)
    if path.exists() and sha256_of(path) == pinned["sha256"]:
        say("cached", path.name)
        return path
    say("download", pinned["url"])
    urllib.request.urlretrieve(pinned["url"], path)
    digest = sha256_of(path)
    if digest != pinned["sha256"]:
        raise RuntimeError(f"{path.name} is {digest}, not the pinned {pinned['sha256']}")
    return path


def install(major):
    """Extracts the archive of `major` (nothing is installed in the Windows sense)."""
    path = fetch(major)
    target = INSTALL / f"pg{major}"
    if not pgbin(major, "postgres.exe").exists():
        say("extract", path.name)
        with zipfile.ZipFile(path) as bundle:
            bundle.extractall(target)
    for program in ("initdb.exe", "pg_ctl.exe", "psql.exe", "postgres.exe"):
        if not pgbin(major, program).exists():
            raise RuntimeError(f"{pgbin(major, program)} is not in the extracted archive")


def step(secret, label, argv, env=None, seconds=600):
    """One command as the dedicated user; its output is printed, a failure raises."""
    code, text = lq.as_user(secret, argv, env=env, seconds=seconds)
    print(f"[{label}] exit {code}\n{text}")
    if code:
        raise RuntimeError(f"{label} failed (exit {code})")
    return text


def provision(secret, passwords):
    """Three clusters, started, each holding `pbps_test`, of its pinned build, under scram-sha-256.

    A cluster is created with trust for its loopback, given its password from a
    temporary SQL file (never on a command line), and then closed to everything
    but scram-sha-256. Nothing runs against it until a connection with the real
    password has succeeded and a connection without one has been refused. The
    password reaches `psql` through `PGPASSWORD`, set in that child's
    environment only: a passfile (`PGPASSFILE`) was observed not to be read by
    `psql` on the hosted runner, while the variable is. That is an observation;
    its cause was not chased.
    """
    DATA.mkdir(parents=True, exist_ok=True)
    for name, (major, port) in CLUSTERS.items():
        data = DATA / name
        step(secret, f"initdb {name}", [pgbin(major, "initdb.exe"), "-D", data, "-U", "postgres",
                                        "--auth=trust", "-E", "UTF8", "--locale=C"])
        step(secret, f"start {name}", [pgbin(major, "pg_ctl.exe"), "start", "-D", data, "-w", "-t", "120",
                                       "-l", DATA / f"{name}.log", "-o", server_arguments(port)])
        script = lq.ROOT / f"{name}-bootstrap.sql"
        script.write_text(bootstrap_sql(passwords[name]), encoding="ascii")
        try:
            step(secret, f"bootstrap {name}", [*psql_argv(major, port), "-v", "ON_ERROR_STOP=1", "-f", script])
        finally:
            script.unlink()
        (data / "pg_hba.conf").write_text(SCRAM_HBA, encoding="ascii")
        step(secret, f"reload {name}", [pgbin(major, "pg_ctl.exe"), "reload", "-D", data])
    for name, (major, port) in CLUSTERS.items():
        text = step(secret, f"scram connect to {name}",
                    [*psql_argv(major, port, DATABASE), "-Atc", "SHOW server_version_num"],
                    env={"PGPASSWORD": passwords[name]})
        print(f"{name}: server_version_num {require_build(text.strip().splitlines()[-1], major)}")
        code, text = lq.as_user(secret, [*psql_argv(major, port, DATABASE), "-Atc", "SELECT 1"])
        if code == 0:
            raise RuntimeError(f"{name} still accepts a connection without its password")
        print(f"{name}: a connection without the password is refused ({text.strip()[:120]})")


def stop(secret):
    for name, (major, _) in CLUSTERS.items():
        data = DATA / name
        if data.exists():
            lq.as_user(secret, [pgbin(major, "pg_ctl.exe"), "stop", "-D", data, "-m", "immediate", "-w", "-t", "60"])


def environment(passwords):
    """The environment the flow cases run with: one connection string per cluster, and the key."""
    return {
        "PBPS_TEST_PG_DB": connection(CLUSTERS["target"][1], passwords["target"]),
        "PBPS_TEST_PG_SCRATCH_DB": connection(CLUSTERS["scratch"][1], passwords["scratch"]),
        "PBPS_TEST_PG_OLD_DB": connection(CLUSTERS["old"][1], passwords["old"]),
        # A key file's owner-only permission cannot be proved on Windows, so
        # the fixtures name this variable instead (flow_pg.rs KEY_ENV).
        "PBPS_TEST_FINGERPRINT_KEY": lq.fingerprint_key(),
        # A debug build on Windows has larger frames than libtest's default
        # thread stack (see localdb_qualify.py).
        "RUST_MIN_STACK": str(8 * 1024 * 1024),
    }


def main():
    results = []
    secret = lq.password()
    passwords = {name: secrets.token_hex(16) for name in CLUSTERS}
    for password in passwords.values():
        print(f"::add-mask::{password}", flush=True)
    for major in PINNED:
        install(major)
    lq.create_user(secret)
    flow = lq.executable(lq.cargo_json(["test", "--no-run", "-p", "pbps-cli", "--test", "flow_pg"]),
                         "pbps-cli", "test", "flow_pg")
    try:
        provision(secret, passwords)
        env = environment(passwords)

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
