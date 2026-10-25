#!/usr/bin/env python3
"""Qualify Windows LocalDB as the SQL Server resolver's other instance (#2271).

Run by the `resolver-localdb` job on a Windows runner; nothing here can run
anywhere else, and the pure helpers are covered by
`scripts/localdb_qualify_test.py` on every platform.

What the job does, in order, and fails on the first thing that does not hold:

1. Installs the pinned SQL Server 2025 (major 17) LocalDB MSI, after checking
   its SHA-256. A different or missing download is a failure, not a skip.
2. Creates one ordinary local user, `pbpslocaldb`. LocalDB instances belong to
   the Windows user that creates them, and the qualification is about what an
   unprivileged user gets, not what the runner's administrator does.
3. Builds the test programs as the administrator, into a directory that user
   can read and run. Nothing is compiled as the user.
4. As that user, creates three private instances: a target, a scratch, and a
   disposable one that the cases that stop and restart an instance may
   disturb (so no case depends on another's order).
5. Resolves the target and scratch to pipe strings with the engine's own
   discovery (`examples/localdb_endpoint.rs`), then runs, as that user:
   the LocalDB cases, and the shared SQL Server resolver suites against the
   same two instances.

A case that does not run is a failure. libtest exits successfully when a
filter selects nothing, so every run is held to the number of cases it listed,
and a run that listed none, or whose summary is not exactly that many passes,
fails the job. Nothing skips, and nothing here continues past an error.
"""

import base64
import hashlib
import json
import os
import re
import secrets
import string
import subprocess
import sys
import urllib.request
from pathlib import Path

# SQL Server 2025 LocalDB, as published on download.microsoft.com. Pinned by
# content: a changed file fails the check below instead of being installed.
MSI_URL = "https://download.microsoft.com/download/dea8c210-c44a-4a9d-9d80-0c81578860c5/ENU/SqlLocalDB.msi"
MSI_SHA256 = "c8cf7455b79f4018c895098392bb80c2b5cba0d1e3ecfa22dd6fad7873894561"
# The qualified major's tools directory (`170` is 17.x); see
# `pbps_mssql::resolver::localdb::helper`, which looks in the same place.
HELPER = Path(os.environ.get("ProgramFiles", r"C:\Program Files")) / "Microsoft SQL Server" / "170" / "Tools" / "Binn" / "SqlLocalDB.exe"

USER = "pbpslocaldb"
ROOT = Path(r"C:\pbps-localdb")
TARGET, SCRATCH, DISPOSABLE = "pbps_target", "pbps_scratch", "pbps_disposable"
# LocalDB has no TLS on its pipe (measured on 17.0.4025.3): a string requiring
# encryption fails with "Server does not allow the requested encryption
# level", and `Encrypt=false` still asks for an encrypted login, so the server
# drops the pipe (os error 233). pbps never downgrades a string, so the
# operator writes the driver's no-TLS spelling, `Encrypt=DANGER_PLAINTEXT`
# (DEC-2271.2). The case that checks the policy asks for encryption itself.
TRUST = "Encrypt=DANGER_PLAINTEXT"

# A plain pattern: the inventory's fixture grammar refuses `re.compile`.
SUMMARY = (r"test result: (ok|FAILED)\. (\d+) passed; (\d+) failed; (\d+) ignored; "
           r"(\d+) measured; (\d+) filtered out")
# cmd.exe would expand or end a value at these, inside quotes or not.
UNSAFE = set('%^"!&|<>\r\n')


def listed(output):
    """The cases `--list --format terse` printed."""
    return [line[:-6] for line in output.splitlines() if line.endswith(": test")]


def require_ran(output, expected, label):
    """Raise unless exactly `expected` cases passed in one run, and expected > 0."""
    if expected <= 0:
        raise RuntimeError(f"{label}: no case was selected, which is not a pass")
    summaries = re.findall(SUMMARY, output)
    if len(summaries) != 1:
        raise RuntimeError(f"{label}: expected one libtest summary, found {len(summaries)}")
    verdict, passed, failed, _, _, _ = summaries[0]
    if verdict != "ok" or failed != "0" or int(passed) != expected:
        raise RuntimeError(f"{label}: {passed} passed, {failed} failed, {expected} expected")


def require_case(output, test):
    """Raise unless `test`, exactly, ran and passed once, as the only case."""
    cases = [line for line in output.splitlines() if re.match(r"test .+ \.\.\. ", line)]
    if cases != [f"test {test} ... ok"]:
        raise RuntimeError(f"{test}: did not run and pass exactly once: {cases}")
    require_ran(output, 1, test)


def selector(name):
    return rf"Server=(localdb)\{name};Integrated Security=SSPI;{TRUST}"


def quote(value):
    """A literal for a `.cmd` line, or an error for anything cmd would reinterpret."""
    bad = sorted(set(value) & UNSAFE)
    if bad:
        raise ValueError(f"unsafe character {bad!r} in a value for cmd.exe")
    return f'"{value}"'


def wrapper(env, argv, stdout, stderr=None):
    """A `.cmd` that sets `env`, runs `argv`, and writes its output to files.

    The user's process does not inherit this one's environment, so the script
    is how the connection strings reach it. They hold pipe names and no secret.
    """
    lines = ["@echo off"]
    lines += [f"set {quote(f'{key}={value}')}" for key, value in sorted(env.items())]
    command = " ".join(quote(str(part)) for part in argv)
    lines.append(f"{command} > {quote(str(stdout))} 2> {quote(str(stderr or stdout))}"
                 if stderr else f"{command} > {quote(str(stdout))} 2>&1")
    lines.append("exit /b %ERRORLEVEL%")
    return "\r\n".join(lines) + "\r\n"


def password():
    """A password meeting the default Windows complexity policy."""
    pools = [string.ascii_lowercase, string.ascii_uppercase, string.digits, "-_.+="]
    chars = [secrets.choice(pool) for pool in pools] + [
        secrets.choice("".join(pools)) for _ in range(28)
    ]
    secrets.SystemRandom().shuffle(chars)
    return "".join(chars)


def executable(messages, package, kind, name=None):
    """The one test executable cargo reported for `package`'s `kind` target."""
    found = []
    for line in messages.splitlines():
        try:
            message = json.loads(line)
        except ValueError:
            continue
        if message.get("reason") != "compiler-artifact" or not message.get("executable"):
            continue
        target = message["target"]
        source = target["src_path"].replace("\\", "/")
        is_test = message.get("profile", {}).get("test")
        if (f"/crates/{package}/" in source and kind in target["kind"]
                and (name is None or target["name"] == name) and (is_test or kind == "example")):
            found.append(message["executable"])
    if len(found) != 1:
        raise RuntimeError(f"expected one {package} {kind} executable, found {found}")
    return found[0]


# Everything below needs Windows.

def say(*parts):
    print("+", *parts, flush=True)


def check(argv, **kwargs):
    say(*argv)
    return subprocess.run(argv, check=True, **kwargs)


def install():
    ROOT.mkdir(exist_ok=True)
    msi = ROOT / "SqlLocalDB.msi"
    if not msi.exists():
        say("download", MSI_URL)
        urllib.request.urlretrieve(MSI_URL, msi)
    digest = hashlib.sha256(msi.read_bytes()).hexdigest()
    if digest != MSI_SHA256:
        raise RuntimeError(f"the LocalDB installer is {digest}, not the pinned {MSI_SHA256}")
    log = ROOT / "install.log"
    done = subprocess.run(["msiexec", "/i", str(msi), "/qn", "IACCEPTSQLLOCALDBLICENSETERMS=YES",
                           "/l*v", str(log)])
    # 3010 is "installed, restart wanted"; LocalDB needs none.
    if done.returncode not in (0, 3010) or not HELPER.exists():
        print(log.read_text(errors="replace")[-4000:] if log.exists() else "no install log")
        raise RuntimeError(f"LocalDB did not install (msiexec {done.returncode}; {HELPER} exists: {HELPER.exists()})")


def windows_powershell_env(secret, base=None):
    """The environment for a Windows PowerShell 5.1 child that receives `secret`.

    The job's steps run under PowerShell 7, which puts its own module
    directories in PSModulePath. Windows PowerShell 5.1 inherits that and then
    cannot load its built-in modules (the first run failed with
    "ConvertTo-SecureString ... the module could not be loaded"). Without the
    variable, 5.1 builds its default module path.
    """
    env = {k: v for k, v in (os.environ if base is None else base).items() if k.upper() != "PSMODULEPATH"}
    env["PBPS_LOCALDB_PASSWORD"] = secret
    return env


def fingerprint_key():
    """A fresh key as `pbps key generate` prints it: 32 random bytes, base64."""
    return base64.b64encode(secrets.token_bytes(32)).decode("ascii")


def create_user_command():
    """The PowerShell that creates the dedicated user from $env:PBPS_LOCALDB_PASSWORD.

    Not `net user /add`: for a password longer than 14 characters it asks for
    confirmation on the console, and with no console it fails ("No valid
    response was provided"). New-LocalUser takes the password from the
    environment, so it never appears on a command line either. The user joins
    `Users` (by SID, whatever the display language calls it) only, never
    `Administrators`; already being a member is not an error.
    """
    return (
        "$ErrorActionPreference = 'Stop';"
        "$p = ConvertTo-SecureString $env:PBPS_LOCALDB_PASSWORD -AsPlainText -Force;"
        f"$null = New-LocalUser -Name '{USER}' -Password $p -AccountNeverExpires "
        "-PasswordNeverExpires -UserMayNotChangePassword;"
        f"try {{ Add-LocalGroupMember -SID 'S-1-5-32-545' -Member '{USER}' }} "
        "catch [Microsoft.PowerShell.Commands.MemberExistsException] { }"
    )


def create_user(secret):
    print(f"::add-mask::{secret}", flush=True)
    say("create user", USER)
    subprocess.run(["powershell", "-NoProfile", "-NonInteractive", "-Command", create_user_command()],
                   check=True, env=windows_powershell_env(secret))
    tmp = Path(r"C:\tmp")  # the shared suites back databases up to `/tmp/...`
    tmp.mkdir(exist_ok=True)
    workspace = Path.cwd()
    for path, rights in ((ROOT, "M"), (tmp, "M"), (workspace, "RX")):
        check(["icacls", str(path), "/grant", f"{USER}:(OI)(CI){rights}", "/T", "/C", "/Q"],
              stdout=subprocess.DEVNULL)


def as_user(secret, argv, env=None, stdout=None, stderr=None, seconds=1800):
    """Runs `argv` as the dedicated user and returns its exit code and output."""
    ROOT.mkdir(exist_ok=True)
    stdout = stdout or ROOT / f"run-{secrets.token_hex(4)}.log"
    script = ROOT / f"{stdout.stem}.cmd"
    script.write_text(wrapper(env or {}, argv, stdout, stderr), encoding="ascii", newline="")
    shell = (
        "$p = ConvertTo-SecureString $env:PBPS_LOCALDB_PASSWORD -AsPlainText -Force;"
        f"$c = New-Object System.Management.Automation.PSCredential(\"$env:COMPUTERNAME\\{USER}\", $p);"
        f"$q = Start-Process -FilePath cmd.exe -ArgumentList '/d','/c','\"{script}\"' -Credential $c "
        f"-LoadUserProfile -WorkingDirectory '{ROOT}' -WindowStyle Hidden -PassThru;"
        "$null = $q.Handle;"
        f"if (-not $q.WaitForExit({seconds * 1000})) {{ taskkill /F /T /PID $q.Id | Out-Null; exit 124 }};"
        "exit $q.ExitCode"
    )
    say("as", USER, *argv[:3])
    code = subprocess.run(["powershell", "-NoProfile", "-NonInteractive", "-Command", shell],
                          env=windows_powershell_env(secret)).returncode
    text = Path(stdout).read_text(errors="replace") if Path(stdout).exists() else ""
    return code, text


def cargo_json(args):
    say("cargo", *args)
    done = subprocess.run(["cargo", *args, "--message-format=json"], stdout=subprocess.PIPE,
                          text=True, encoding="utf-8", env={**os.environ, "CARGO_TARGET_DIR": str(ROOT / "target")})
    if done.returncode:
        raise RuntimeError(f"cargo {' '.join(args)} failed")
    return done.stdout


def main():
    results = []
    secret = password()
    install()
    create_user(secret)
    live = executable(cargo_json(["test", "--no-run", "-p", "pbps-mssql", "--test", "live"]),
                      "pbps-mssql", "test", "live")
    cli = executable(cargo_json(["test", "--no-run", "-p", "pbps-cli", "--lib"]),
                     "pbps-cli", "lib")
    example = executable(cargo_json(["build", "-p", "pbps-mssql", "--example", "localdb_endpoint"]),
                         "pbps-mssql", "example", "localdb_endpoint")

    for name in (TARGET, SCRATCH, DISPOSABLE):
        code, text = as_user(secret, [HELPER, "create", name, "-s"])
        print(text)
        if code:
            raise RuntimeError(f"SqlLocalDB.exe could not create and start {name} (exit {code})")
    # Printed for the record, never judged: what the helper says, as the
    # dedicated user, for an instance that does not exist and for its list.
    for argv in ([HELPER, "info", "pbps_never_created"], [HELPER, "info"]):
        code, text = as_user(secret, argv)
        print(f"{' '.join(str(a) for a in argv[1:])}: exit {code}\n{text}")
    resolved = {}
    for name in (TARGET, SCRATCH):
        code, text = as_user(secret, [example, selector(name)],
                             stdout=ROOT / f"endpoint-{name}.txt", stderr=ROOT / f"endpoint-{name}.err")
        error = (ROOT / f"endpoint-{name}.err").read_text(errors="replace")
        if code or not text.strip():
            raise RuntimeError(f"discovery of {name} failed (exit {code}): {error}")
        resolved[name] = text.strip()
    env = {
        "PBPS_TEST_DB": resolved[TARGET],
        "PBPS_TEST_MSSQL_SCRATCH_DB": resolved[SCRATCH],
        "PBPS_TEST_LOCALDB_SCRATCH": selector(SCRATCH),
        "PBPS_TEST_LOCALDB_DISPOSABLE": selector(DISPOSABLE),
        # A key file's owner-only permission cannot be proved on Windows, so
        # the fixtures name this variable instead (fixtures.rs KEY_LINE).
        "PBPS_TEST_FINGERPRINT_KEY": fingerprint_key(),
    }

    def run_cases(binary, label, filters, minimum, exact=False):
        """One libtest run per filter, held to the cases it listed."""
        for filter_ in filters:
            args = [filter_] + (["--exact"] if exact else [])
            listing = subprocess.run([binary, "--list", "--ignored", "--format", "terse", *args],
                                     check=True, text=True, stdout=subprocess.PIPE).stdout
            cases = listed(listing)
            if len(cases) < minimum:
                raise RuntimeError(f"{label} {filter_}: listed {len(cases)} cases, fewer than the {minimum} qualified")
            code, text = as_user(secret, [binary, "--ignored", "--test-threads=1", "--format=pretty",
                                          "--color=never", *args], env=env)
            print(text)
            require_ran(text, len(cases), f"{label} {filter_}")
            if code:
                raise RuntimeError(f"{label} {filter_} exited {code}")
            results.append((label, filter_, len(cases)))

    def run_case(binary, test, *extra):
        code, text = as_user(secret, [binary, "--ignored", "--exact", test, "--test-threads=1",
                                      "--format=pretty", "--color=never", *extra], env=env)
        print(text)
        require_case(text, test)
        if code:
            raise RuntimeError(f"{test} exited {code}")
        results.append(("localdb", test, 1))

    # The cases that exist because the scratch is reached by discovery.
    run_case(cli, "resolver::server::scratch::localdb_live_tests::localdb2271_two_instances_are_distinct_live_instances_of_one_build_run_as_the_dedicated_user")
    # Printed, not only asserted: the log records the token the check admits.
    run_case(cli, "resolver::server::scratch::localdb_live_tests::localdb2271_a_windows_owners_dbo_token_adds_only_deny_only_windows_groups",
             "--show-output")
    run_case(cli, "resolver::server::scratch::localdb_live_tests::localdb2271_a_selector_run_reaches_reconstruction_and_leaves_both_instances_clean")
    run_case(cli, "resolver::server::scratch::localdb_live_tests::localdb2271_the_marker_separates_instances_and_same_instance_controls_hold")
    run_case(cli, "resolver::server::scratch::localdb_live_tests::localdb2271_the_requested_certificate_policy_is_kept_over_the_pipe")
    run_case(cli, "resolver::server::scratch::localdb_live_tests::localdb2271_a_missing_or_stopped_instance_fails_by_name_within_the_bound")
    run_case(cli, "resolver::server::scratch::localdb_live_tests::localdb2271_a_restart_invalidates_the_old_run_and_a_fresh_run_requalifies")

    # The shared suites, against the same two instances. Their SQL-login cases
    # cannot run here by design: a pipe is integrated authentication only.
    run_cases(live, "pbps-mssql live", ["scratch2236::", "deployer2267::", "staged2268::", "externals2269::"], 5)
    run_cases(cli, "pbps-cli mssql", [f"resolver::server::scratch::mssql_live_tests::{case}" for case in SHARED_CLI], 1, exact=True)

    if not results or sum(count for *_, count in results) == 0:
        raise RuntimeError("no qualification case ran")
    for label, name, count in results:
        print(f"{count:4d}  {label}  {name}")
    print("LocalDB qualification: all cases passed")


# The `mssql_live_tests` cases that open no SQL login and need no Docker.
SHARED_CLI = [
    "mssql_dbo_compile_session_keeps_the_marker_and_cleans_through_a_fresh_sandbox",
    "mssql_supplied_cleanup_names_the_retained_sandbox_after_failure",
    "mssql_supplied_acquisition_refuses_target_and_cleans_after_the_externals_refusal",
    "instance_wide_ownership_chaining_refuses_the_scratch_sandbox_and_cleans_setup",
    "mssql_run_owned_acquisition_removes_its_login_database_and_reference_on_refusal",
    "mssql_supplied_guest_admission_dirty_inventory_and_persistent_options_refuse_by_name",
    "mssql_externals_file_failure_and_partial_commit_release_owned_resources",
    "mssql_externals_file_is_compared_then_reaches_reconstruction_refusal",
    "mssql_prepare_renders_missing_external_and_returns_entered_compile_connection",
    "mssql_supplied_type_permissions_release_to_empty_inventory",
]


if __name__ == "__main__":
    main()
