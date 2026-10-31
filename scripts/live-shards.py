#!/usr/bin/env python3
"""Run the repository live runners in isolated shards and audit their receipts."""

import argparse
from collections import Counter
from concurrent.futures import ThreadPoolExecutor
import datetime
import hashlib
import json
import math
import os
from pathlib import Path
import re
import shutil
import signal
from statistics import median
import subprocess
import sys
import tempfile
from threading import Event, Lock, Thread
import time
import uuid


ROOT = Path(__file__).resolve().parents[1]
SCRIPT = Path(__file__).resolve()
RUNNERS = {"mssql": "live-tests.sh", "pg": "live-tests-pg.sh"}
MSSQL_SET_GIB = 9
MSSQL_WITHOUT_EXPRESS_GIB = 4
PG_SHARD_GIB = 1
MEMORY_SAMPLE_SECONDS = 15
# Allow thirty seconds for helper finally blocks to remove Docker containers
# and volumes, while bounding interruption if a helper cannot finish cleanup.
RUNNER_STOP_GRACE_SECONDS = 30
EPHEMERAL_RANGE = Path("/proc/sys/net/ipv4/ip_local_port_range")
EXPRESS_INVENTORY = {
    ("test", "-p", "pbps-mssql", "--test", "live", "--"): {
        "binding619::a_global_temporary_table_marks_one_instance_to_every_login_on_it",
        "scratch2236::an_express_supplied_reference_predicts_a_fresh_database_exactly",
        "scratch2236::an_express_database_closed_before_its_read_still_reads_as_open",
        "resolver_tests::editions611::a_real_express_target_is_an_edition_limitation_and_its_server_collation_a_mismatch",
    },
    ("test", "-p", "pbps-cli", "--test", "flow", "--"): {
        "connected_plan_json_reports_unsupported_online_edition_as_a_finding",
    },
}
EXPRESS_TESTS = set().union(*EXPRESS_INVENTORY.values())
MANIFEST = "manifest.json"
REGISTER = "scripts/known-flakes.json"
# A registered flake that fails a third time on one head is not flaky (DEC-2553.1).
MAX_FLAKE_FAILURES = 2
# Default-include: any variable with these spellings may change a verdict.
INPUT_PREFIXES = ("PBPS_", "INSTA_", "CARGO_PROFILE_")
INPUT_NAMES = {"RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS", "RUSTDOCFLAGS"}
FIXTURES = {
    "mssql": [("PBPS_TEST_MSSQL_NAME", "PBPS_TEST_PORT", "mssql", 26330),
              ("PBPS_TEST_MSSQL_EXPRESS_NAME", "PBPS_TEST_EXPRESS_PORT", "mssql-express", 26331),
              ("PBPS_TEST_MSSQL_SCRATCH_NAME", "PBPS_TEST_MSSQL_SCRATCH_PORT", "mssql-scratch", 26332)],
    "pg": [("PBPS_TEST_PG_NAME", "PBPS_TEST_PG_PORT", "pg", 27320),
           ("PBPS_TEST_PG_OLD_NAME", "PBPS_TEST_PG_OLD_PORT", "pg16", 27321),
           ("PBPS_TEST_PG_SCRATCH_NAME", "PBPS_TEST_PG_SCRATCH_PORT", "pg-scratch", 27322)],
}
# Fixture identity is chosen per shard, so it cannot change what a test proves.
FIXTURE_VARIABLES = {v for fixtures in FIXTURES.values() for name, port, _, _ in fixtures
                     for v in (name, port)}


def require(condition, message):
    if not condition:
        raise ValueError(message)


def selector(value):
    require(re.fullmatch(r"[0-9]+/[1-9][0-9]*", value), "shard must be zero-based i/N")
    index, count = map(int, value.split("/"))
    require(index < count, "shard index must be less than its count")
    return index, count


def fixture_env(suite, index, source):
    env = dict(source)
    prefix = env.get("PBPS_LIVE_PREFIX") or "pbps-live"
    low, high = ephemeral_ports()
    for name, port, kind, default in FIXTURES[suite]:
        base = env.get(name)
        env[name] = f"{base}-s{index}" if base else f"{prefix}s{index}-{kind}"
        if name == "PBPS_TEST_MSSQL_SCRATCH_NAME" and not base:
            env[name] = env["PBPS_TEST_MSSQL_NAME"] + "-scratch"
        require(re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9_.-]*", env[name]), "invalid fixture name")
        env[port] = str(int(env.get(port) or default) + 10 * index)
        require(0 < int(env[port]) < 65536, "fixture port outside 1..65535")
        require(not low <= int(env[port]) <= high,
                f"fixture {env[name]}: {port}={env[port]} overlaps host ephemeral range {low}..{high}")
    require(len({env[n] for n, _, _, _ in FIXTURES[suite]}) == 3, "fixture names collide")
    require(len({env[p] for _, p, _, _ in FIXTURES[suite]}) == 3, "fixture ports collide")
    return env


def ephemeral_ports():
    try:
        text = EPHEMERAL_RANGE.read_text()
    except OSError as error:
        raise ValueError(f"cannot read host ephemeral port range {EPHEMERAL_RANGE}: {error}") from error
    require(re.fullmatch(r"\s*[0-9]+\s+[0-9]+\s*", text), "unreadable host ephemeral port range")
    low, high = map(int, text.split())
    require(0 < low <= high < 65536, "invalid host ephemeral port range")
    return low, high


def container_names():
    result = subprocess.run(["docker", "ps", "-a", "--format", "{{.Names}}"],
                            text=True, capture_output=True)
    require(result.returncode == 0, f"cannot list fixture containers: {result.stderr.strip()}")
    names = result.stdout.splitlines()
    require(all(re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9_.-]*", name) for name in names),
            "unreadable fixture container names")
    return set(names)


class FixtureRun:
    """Own only a fresh assignment, and stop its writers before removing it."""

    def __init__(self, names):
        self.names = set(names)
        self.processes = []
        self.lock = Lock()
        self.stopped = False
        self.handlers = {}

    def __enter__(self):
        # Reusing a container would make an assignment insufficient proof of
        # ownership. Canonical runners may reuse; the whole-run driver may not.
        existing = self.names.intersection(container_names())
        require(not existing, "fixture containers already exist: " + ", ".join(sorted(existing)))
        for sig in (signal.SIGINT, signal.SIGTERM):
            self.handlers[sig] = signal.signal(sig, self.interrupt)
        return self

    def run(self, command, **kwargs):
        with self.lock:
            require(not self.stopped, "fixture run interrupted")
            process = subprocess.Popen(command, start_new_session=True, **kwargs)
            self.processes.append(process)
        try:
            return process.wait()
        finally:
            with self.lock:
                self.processes.remove(process)

    def stop(self):
        with self.lock:
            self.stopped = True
            processes = list(self.processes)
        if not processes:
            return
        deadline = time.monotonic() + RUNNER_STOP_GRACE_SECONDS
        for process in processes:
            try:
                os.killpg(process.pid, signal.SIGTERM)
            except ProcessLookupError:
                pass
        for process in processes:
            try:
                process.wait(timeout=max(0, deadline - time.monotonic()))
            except subprocess.TimeoutExpired:
                pass
        # A runner can exit before its TLS helper finishes removing its own
        # container. Keep the whole group alive for cleanup (DEC-2366.1).
        groups = [process.pid for process in processes]
        while groups:
            remaining = []
            for pid in groups:
                try:
                    os.killpg(pid, 0)
                    remaining.append(pid)
                except ProcessLookupError:
                    pass
            groups = remaining
            delay = deadline - time.monotonic()
            if not groups or delay <= 0:
                break
            time.sleep(min(0.05, delay))
        for pid in groups:
            try:
                os.killpg(pid, signal.SIGKILL)
            except ProcessLookupError:
                pass

    def interrupt(self, sig, frame):
        for handled in self.handlers:
            signal.signal(handled, signal.SIG_IGN)
        self.stop()
        raise InterruptedError(f"fixture run interrupted by {signal.Signals(sig).name}")

    def __exit__(self, *exception):
        # A repeated terminal signal must not interrupt removal halfway through.
        for sig in self.handlers:
            signal.signal(sig, signal.SIG_IGN)
        try:
            self.stop()
            remaining = self.names.intersection(container_names())
            errors = []
            for name in sorted(remaining):
                try:
                    result = subprocess.run(["docker", "rm", "-f", "-v", name],
                                            text=True, capture_output=True)
                    if result.returncode:
                        errors.append(f"{name}: {result.stderr.strip()}")
                except OSError as error:
                    errors.append(f"{name}: {error}")
            require(not errors, "fixture cleanup failed: " + "; ".join(errors))
        finally:
            for sig, handler in self.handlers.items():
                signal.signal(sig, handler)


def partition(names, count, identity, weights, pinned=False, *, round_robin=False):
    require(len(set(names)) == len(names), "duplicate name in --list output")
    groups = [[] for _ in range(count)]
    if round_robin:
        for index, name in enumerate(sorted(names)):
            groups[index % count].append(name)
    elif pinned:
        groups[0] = list(names)
    elif weights is not None:
        loads = [0.0] * count
        fallback = median(weights.values()) if weights else 1.0
        groups[0] = sorted(name for name in names if name in EXPRESS_TESTS)
        loads[0] = sum(weights.get(identity + ":" + name, fallback) for name in groups[0])
        remaining = [name for name in names if name not in EXPRESS_TESTS]
        for name in sorted(remaining, key=lambda n: (-weights.get(identity + ":" + n, fallback), n)):
            index = min(range(count), key=lambda i: (loads[i], i))
            groups[index].append(name)
            loads[index] += weights.get(identity + ":" + name, fallback)
    else:
        for name in names:
            digest = hashlib.sha256((identity + ":" + name).encode()).digest()
            index = 0 if name in EXPRESS_TESTS else int.from_bytes(digest, "big") % count
            groups[index].append(name)
    verify_union(names, groups)
    return [sorted(group) for group in groups]


def verify_express(groups):
    require(not EXPRESS_TESTS.intersection(name for group in groups[1:] for name in group),
            "Express-needing test assigned outside shard zero")


def preflight_express(count, weights, env):
    # These are fixture inventories, not an alternative runner command list.
    # Resolve exact names before any runner provisions a database container.
    real = shutil.which("cargo", path=env.get("PATH"))
    require(real is not None, "cargo is not installed")
    for cargo, expected in EXPRESS_INVENTORY.items():
        result = subprocess.run([real, *cargo, "--ignored", "--list", "--format=pretty", "--color=never"],
                                env=env, cwd=ROOT, text=True, stdout=subprocess.PIPE)
        require(result.returncode == 0, "Express fixture inventory failed")
        names = listed_tests(result.stdout)
        require(expected.issubset(names), "Express fixture inventory lacks a classified exact name")
        identity = hashlib.sha256(json.dumps(list(cargo)).encode()).hexdigest()[:24]
        verify_express(partition(names, count, identity, weights))
    return sorted(EXPRESS_TESTS)


def verify_union(names, groups):
    require(Counter(name for group in groups for name in group) == Counter(names),
            "shard union differs from --list (missing, extra or duplicate tests)")


def read_weights(path):
    if not path:
        return None
    file = Path(path)
    if not file.exists():
        require(not file.is_symlink(), "weights path is an unreadable dangling symlink")
        return None
    weights = json.loads(file.read_text())
    require(isinstance(weights, dict), "weights must be a JSON object")
    require(all(isinstance(k, str) and type(v) in (float, int) and math.isfinite(v) and v > 0
                for k, v in weights.items()), "weights must contain positive finite seconds")
    return weights


def libtest_args(args):
    require(args[:1] == ["test"] and "--" in args, "sharding requires cargo test with --")
    split = args.index("--")
    cargo, flags, listing = args[:split + 1], [], []
    tail = iter(args[split + 1:])
    for arg in tail:
        if arg in ("--skip", "--test-threads", "--format", "--color"):
            value = next(tail, None)
            require(value is not None, f"missing value for {arg}")
            if arg == "--skip":
                flags.extend([arg, value])
                listing.extend([arg, value])
        elif arg.startswith("--skip=") or arg in ("--ignored", "--include-ignored"):
            flags.append(arg)
            listing.append(arg)
        elif arg == "--exact" or arg.startswith(("--test-threads=", "--format=", "--color=")):
            if arg == "--exact":
                listing.append(arg)
        else:
            # Positive filters are used by enumeration, then replaced by the
            # assigned exact names. Keeping them would OR extra tests back in.
            require(not arg.startswith("-"), f"unsupported shard-mode libtest option: {arg}")
            listing.append(arg)
    return cargo, flags, listing


def listed_tests(output):
    names = re.findall(r"^(.+): test$", output, re.MULTILINE)
    summaries = re.findall(r"^([0-9]+) tests?, ([0-9]+) benchmarks?$", output, re.MULTILINE)
    require(len(summaries) == 1 and int(summaries[0][0]) == len(names)
            and int(summaries[0][1]) == 0, "unreadable or incomplete libtest --list output")
    require(len(set(names)) == len(names), "duplicate test in --list")
    return names


def git_output(root, *args):
    result = subprocess.run(["git", *args], cwd=root, text=True, capture_output=True)
    require(result.returncode == 0, f"git {args[0]} failed: {result.stderr.strip()}")
    return result.stdout


def tree_identity(root):
    """HEAD's tree id when the worktree is exactly that tree, else None (DEC-2553.1).

    An untracked file counts as a change: Cargo discovers `tests/*.rs` by name.
    `target` is excluded because gate worktrees symlink it to the build disk and
    `.gitignore` names it as a directory, so the link itself reads as untracked.
    """
    try:
        head = git_output(root, "rev-parse", "--verify", "HEAD^{tree}").strip()
        dirty = git_output(root, "status", "--porcelain", "--untracked-files=all",
                           "--", ".", ":(exclude)target")
    except (ValueError, OSError):
        return None
    return head if re.fullmatch(r"[0-9a-f]{40,64}", head) and not dirty else None


def inputs_digest(env):
    """Digest of what a verdict depends on and the head tree cannot prove.

    The tree proves every tracked file, including the lockfile, the runners and
    the image digests pinned in them. The toolchain that built the tests and the
    environment the runners and tests read are outside it.
    """
    def tool(command):
        result = subprocess.run(command, env=env, cwd=ROOT, text=True, capture_output=True)
        require(result.returncode == 0, f"{command[0]} failed: {result.stderr.strip()}")
        return result.stdout
    names = sorted(name for name in env
                   if name not in FIXTURE_VARIABLES and not name.startswith("PBPS_LIVE_")
                   and (name in INPUT_NAMES or name.startswith(INPUT_PREFIXES)))
    # rustc's commit and host identify the toolchain; cargo ships with it.
    body = {"version": 1, "rustc": tool(["rustc", "-vV"]),
            "env": {name: env[name] for name in names}}
    return hashlib.sha256(json.dumps(body, sort_keys=True).encode()).hexdigest(), names


def write_atomic(path, text):
    # A killed writer must not leave a truncated receipt that a resume would refuse.
    temporary = path.with_name(path.name + ".tmp")
    temporary.write_text(text)
    os.replace(temporary, path)


def read_json(path, what):
    try:
        return json.loads(path.read_text())
    except (OSError, ValueError) as error:
        raise ValueError(f"unreadable {what} {path}: {error}") from error


def check_row(row, path):
    names = lambda value: isinstance(value, list) and all(isinstance(n, str) for n in value)
    require(isinstance(row, dict) and all(
        key in row for key in ("args", "listed", "assigned", "reported", "valid", "tree", "inputs", "carried")),
        f"receipt {path} lacks the head identity or a field")
    require(names(row["args"]) and names(row["listed"]) and names(row["assigned"])
            and isinstance(row["valid"], bool) and isinstance(row["reported"], dict)
            and isinstance(row["carried"], dict), f"malformed receipt {path}")
    require(all(isinstance(r, dict) and r.get("status") in ("ok", "FAILED") for r in row["reported"].values())
            and all(isinstance(r, dict) and r.get("status") == "ok" for r in row["carried"].values()),
            f"malformed result in receipt {path}")


def load_resume(previous, tree, inputs, suites, flakes):
    """Read what a previous run on this head proved; refuse anything else (DEC-2553.1).

    A missing receipt only means a name is not proven yet. A receipt that is
    unreadable, malformed, from another head or inputs, or that disagrees with
    its siblings refuses the whole resume. `flakes` maps (cargo, name) to its
    register entry. Returns (proven, failures, waived): proven names per suite
    and invocation key, FAILED totals per head, and the register entries used.
    """
    previous = Path(previous)
    manifest = read_json(previous / MANIFEST, "manifest")
    require(isinstance(manifest, dict) and manifest.get("version") == 1
            and isinstance(manifest.get("run_id"), str), "unsupported previous manifest")
    for name, value in (("tree", tree), ("inputs", inputs)):
        require(isinstance(manifest.get(name), str) and manifest[name],
                f"the previous run has no {name} identity (dirty or unreadable)")
        require(value is not None, f"this run has no {name} identity (dirty or unreadable)")
        require(manifest[name] == value, f"previous {name} {manifest[name][:12]} != current {value[:12]}"
                + (f" (environment: {', '.join(manifest.get('inputs_env', []))})" if name == "inputs" else ""))
    require(sorted(manifest.get("suites", [])) == sorted(suites),
            f"the previous run covered {manifest.get('suites')}, not {suites}")
    history = manifest.get("failed", {})
    require(isinstance(history, dict), "malformed failure history")
    proven, failures, waived, refused = {}, {}, [], []
    for suite in suites:
        directory = previous / suite
        require(directory.is_dir(), f"the previous output has no {suite} receipts")
        rows = {}
        for path in sorted(directory.glob("*/*.json")):
            row = read_json(path, "receipt")
            check_row(row, path)
            require(row["tree"] == tree and row["inputs"] == inputs,
                    f"receipt {path} was written for another head or inputs")
            rows.setdefault(path.stem, []).append(row)
        for key, group in rows.items():
            first = group[0]
            require(all(row["args"] == first["args"] and sorted(row["listed"]) == sorted(first["listed"])
                        and row["carried"] == first["carried"] for row in group),
                    f"receipts of invocation {key} disagree")
            ok = dict(first["carried"])
            counts = dict(history.get(suite, {}).get(key, {}))
            cargo = tuple(libtest_args(first["args"])[0])
            failed = set()
            for row in group:
                for name, result in row["reported"].items():
                    if result["status"] == "FAILED":
                        failed.add(name)
                        counts[name] = counts.get(name, 0) + 1
                        flake = flakes.get((cargo, name))
                        if flake is None or counts[name] > MAX_FLAKE_FAILURES:
                            refused.append(f"{suite}: {name}")
                        else:
                            waived.append(flake)
                    elif row["valid"]:
                        ok[name] = {"status": "ok", "seconds": result["seconds"], "run": manifest["run_id"]}
            for name in failed:
                ok.pop(name, None)
            proven.setdefault(suite, {})[key] = {"listed": sorted(first["listed"]), "ok": ok}
            failures.setdefault(suite, {})[key] = {n: c for n, c in counts.items() if c}
        for key, counts in history.get(suite, {}).items():
            failures.setdefault(suite, {}).setdefault(key, counts)
    require(not refused, "FAILED outside the unexpired known-flake register, or failed more than "
            f"{MAX_FLAKE_FAILURES} times on this head: " + ", ".join(sorted(set(refused))))
    return proven, failures, waived, manifest


def run_tests(command, assigned):
    reported, summaries, duplicates, headers = {}, [], [], []
    active = False
    started = last = time.monotonic()
    # The commutation harness writes directly to stderr during passing tests.
    # Merging it here splits libtest's partial "test name ... " stdout line.
    with subprocess.Popen(command, stdout=subprocess.PIPE, stderr=None,
                          text=True, bufsize=1) as process:
        for line in process.stdout:
            print(line, end="", flush=True)
            header = re.fullmatch(r"running ([0-9]+) tests?\n?", line)
            if header:
                headers.append(int(header[1]))
                active = True
            if line.strip() == "failures:" or line.startswith("test result:"):
                active = False
            match = re.fullmatch(r"test (.+) \.\.\. (ok|FAILED)\n?", line)
            if match and active:
                name, status = match.groups()
                if name in reported:
                    duplicates.append(name)
                now = time.monotonic()
                reported[name] = {"status": status, "seconds": max(now - last, 0.000001)}
                last = now
            if line.startswith("test result:"):
                summaries.append(line.strip())
        code = process.wait()
    passed = sum(r["status"] == "ok" for r in reported.values())
    failed = len(reported) - passed
    valid = (headers == [len(assigned)] and not duplicates and set(reported) == set(assigned)
        and len(summaries) == 1 and re.fullmatch(
        rf"test result: {'FAILED' if failed else 'ok'}\. {passed} passed; {failed} failed; "
        r"0 ignored; 0 measured; [0-9]+ filtered out; finished in .+", summaries[0])
        and (code == 0) == (failed == 0))
    return reported, code, time.monotonic() - started, bool(valid)


def ci_shard(value, args, *, expected_tests=None, output=None, allow_empty=False):
    """Use CI's existing services; this path never owns fixture containers."""
    index, count = selector(value)
    require(not allow_empty or output is not None, "empty CI shares require audited receipts")
    cargo, flags, listing = libtest_args(args)
    require("--ignored" in listing and "--include-ignored" not in listing,
            "CI shards require --ignored tests")
    real = shutil.which("cargo")
    require(real is not None, "cargo is not installed")
    enumeration = subprocess.run([real, *cargo, *listing, "--list", "--format=pretty", "--color=never"],
                                 text=True, stdout=subprocess.PIPE)
    require(enumeration.returncode == 0, "cargo --list failed")
    names = listed_tests(enumeration.stdout)
    require(names, "CI --ignored --list contains zero tests")
    if expected_tests is not None:
        require(len(names) == expected_tests,
                f"expected {expected_tests} tests, listed {len(names)}")
    groups = partition(names, count, "", None, round_robin=True)
    assigned = groups[index]
    print(f"ci shard {index}/{count}: {len(assigned)}/{len(names)} tests", flush=True)
    require(assigned or allow_empty, "CI shard has no assigned tests")
    reported, code, seconds, valid = {}, 0, 0.0, True
    if assigned:
        reported, code, seconds, valid = run_tests(
            [real, *cargo, *flags, "--exact", "--test-threads=1", "--format=pretty",
             "--color=never", *assigned], assigned)
    if output is not None:
        root = Path(output) / str(index)
        root.mkdir(parents=True, exist_ok=True)
        key = hashlib.sha256(json.dumps(args).encode()).hexdigest()[:24]
        path = root / (key + ".json")
        require(not path.exists(), "CI invocation already has a receipt")
        receipt = {"args": args, "identity": key, "listed": names, "groups": groups,
                   "assigned": assigned, "reported": reported, "pinned": False,
                   "seconds": seconds, "valid": valid}
        path.write_text(json.dumps(receipt, indent=2) + "\n")
    require(valid, "assigned tests or their libtest summary were not reported exactly once")
    return code


def cargo_proxy(args):
    env = os.environ
    index, count = selector(env["PBPS_LIVE_SHARD"])
    root = Path(env["PBPS_LIVE_SHARD_DIR"])
    real = env["PBPS_LIVE_REAL_CARGO"]
    cargo, flags, listing = libtest_args(args)
    identity = hashlib.sha256(json.dumps(cargo).encode()).hexdigest()[:24]
    invocation = hashlib.sha256(json.dumps(args).encode()).hexdigest()[:24]
    # TLS repeats one exact command with three different trust inputs. Preserve
    # each occurrence as its own obligation instead of overwriting its receipt.
    occurrence = len(list(root.glob(invocation + "-*.json")))
    key = f"{invocation}-{occurrence}"
    enumeration = subprocess.run([real, *cargo, *listing, "--list", "--format=pretty", "--color=never"],
                                 text=True, stdout=subprocess.PIPE)
    require(enumeration.returncode == 0, "cargo --list failed")
    names = listed_tests(enumeration.stdout)
    pinned = env.get("PBPS_LIVE_PINNED") == "1"
    carried, remaining = resumed(env, key, names), names
    if carried:
        remaining = [name for name in names if name not in carried]
    groups = partition(remaining, count, identity, read_weights(env.get("PBPS_LIVE_WEIGHTS")), pinned)
    verify_express(groups)
    assigned = groups[index]
    print(f"shard {index}/{count} {key}: {len(assigned)}/{len(names)} tests", flush=True)
    reported, code, seconds, valid = {}, 0, 0.0, True
    if assigned:
        reported, code, seconds, valid = run_tests(
            [real, *cargo, *flags, "--exact", "--test-threads=1", "--format=pretty",
             "--color=never", *assigned], assigned)
    # The head the tests ran on must still be the head the driver started on.
    tree = tree_identity(ROOT)
    receipt = {"args": args, "identity": identity, "listed": names, "groups": groups,
               "assigned": assigned, "reported": reported, "pinned": pinned, "seconds": seconds,
               "valid": valid, "tree": tree if tree == env.get("PBPS_LIVE_TREE") else None,
               "inputs": env.get("PBPS_LIVE_INPUTS"), "carried": carried}
    write_atomic(root / (key + ".json"), json.dumps(receipt, indent=2) + "\n")
    require(valid, "assigned tests or their libtest summary were not reported exactly once")
    return code


def resumed(env, key, names):
    """The names an earlier run of this head already proved for this invocation."""
    path = env.get("PBPS_LIVE_RESUME")
    if not path:
        return {}
    snapshot = read_json(Path(path), "resume snapshot")
    require(isinstance(snapshot, dict) and isinstance(snapshot.get("keys"), dict),
            "unreadable resume snapshot")
    entry = snapshot["keys"].get(env["PBPS_LIVE_SUITE"], {}).get(key)
    if entry is None:
        return {}
    require(sorted(names) == entry["listed"], "the test list changed since the previous run")
    return {name: entry["ok"][name] for name in names if name in entry["ok"]}


def shard(suite, extra):
    index, count = selector(os.environ["PBPS_LIVE_SHARD"])
    env = fixture_env(suite, index, os.environ)
    # Shell guards for obligations pinned to shard zero need the same index
    # spelling as the integer used by the partition, including accepted zeros.
    env["PBPS_LIVE_SHARD"] = f"{index}/{count}"
    root = Path(env.get("PBPS_LIVE_SHARD_DIR") or tempfile.mkdtemp(prefix="pbps-live-shard-"))
    root.mkdir(parents=True, exist_ok=True)
    require(not list(root.glob("*.json")), "shard output directory already has receipts")
    real = shutil.which("cargo")
    require(real, "cargo is not available")
    proxy = root / "bin" / "cargo"
    proxy.parent.mkdir()
    proxy.write_text(f"#!{sys.executable}\nimport os, sys\nos.execv({sys.executable!r}, "
                     f"[{sys.executable!r}, {str(SCRIPT)!r}, 'cargo', *sys.argv[1:]])\n")
    proxy.chmod(0o755)
    env.update(PBPS_LIVE_ACTIVE="1", PBPS_LIVE_REAL_CARGO=real, PBPS_LIVE_SUITE=suite,
               PBPS_LIVE_SHARD_DIR=str(root), PATH=str(proxy.parent) + os.pathsep + env["PATH"])
    result = subprocess.run(["bash", str(ROOT / "scripts" / RUNNERS[suite]), *extra], cwd=ROOT, env=env)
    receipts = [json.loads(p.read_text()) for p in root.glob("*.json")]
    # A resumed shard may have nothing left; it must still have run every invocation.
    require(receipts if env.get("PBPS_LIVE_RESUME") else any(r["assigned"] for r in receipts),
            "shard is empty for the whole suite")
    return result.returncode


def audit(suite_root, count, *, expected_invocations=None):
    receipts = [{p.stem: json.loads(p.read_text()) for p in (suite_root / str(i)).glob("*.json")}
                for i in range(count)]
    if expected_invocations is not None:
        require(all(len(shard) == expected_invocations for shard in receipts),
                "unexpected invocation receipt count")
    # A resumed run may leave a shard nothing to run; it must still have receipts.
    resumed = any(row.get("carried") for shard in receipts for row in shard.values())
    require(all(shard if resumed else any(r["assigned"] for r in shard.values()) for shard in receipts),
            "a shard is empty for the whole suite")
    passed = failed = carried_total = 0
    measured = {}
    for key in sorted(set().union(*(set(shard) for shard in receipts))):
        rows = [shard.get(key) for shard in receipts]
        reference = next(row for row in rows if row is not None)
        require(all(row is not None for row in rows) or
                reference["pinned"] and rows[0] is not None
                and all(row is None for row in rows[1:]), "missing invocation receipt")
        carried = reference.get("carried", {})
        require(isinstance(carried, dict) and all(isinstance(r, dict) and r.get("status") == "ok"
                                                    for r in carried.values()),
                "a carried name is not ok")
        groups = []
        for index, row in enumerate(rows):
            if row is None:
                groups.append([])
                continue
            require(all(row[k] == reference[k] for k in ("args", "identity", "listed", "groups", "pinned")),
                    "shards disagree about the invocation or partition")
            require(row.get("carried", {}) == carried, "shards disagree about the carried names")
            require(row["assigned"] == row["groups"][index], "assignment differs from partition")
            require(row["valid"], "libtest reports or summary are incomplete")
            require(set(row["assigned"]) == set(row["reported"]), "assigned test has no result")
            require(all(r["status"] in ("ok", "FAILED") for r in row["reported"].values()),
                    "test did not report ok/FAILED")
            groups.append(row["assigned"])
            for name, result in row["reported"].items():
                passed += result["status"] == "ok"
                failed += result["status"] == "FAILED"
                measured[row["identity"] + ":" + name] = result["seconds"]
        require(not set(carried).intersection(name for group in groups for name in group),
                "a carried name ran again")
        verify_union(reference["listed"], [*groups, sorted(carried)])
        verify_express(groups)
        passed += len(carried)
        carried_total += len(carried)
    result = {"passed": passed, "failed": failed, "union": "exact"}
    if carried_total:
        result["carried"] = carried_total
    return result, measured


def register_flakes(ref):
    """The unexpired register entries of `ref`, keyed by (cargo prefix, test name)."""
    import known_flakes
    entries = known_flakes.load(git_output(ROOT, "show", f"{ref}:{REGISTER}"))
    return known_flakes.active(entries, datetime.datetime.now(datetime.timezone.utc).date())


def available_gb():
    # Linux's MemAvailable includes reclaimable cache; MemFree does not.
    output = subprocess.check_output(["free", "-m"], text=True)
    rows = [line.split() for line in output.splitlines() if line.startswith("Mem:")]
    require(len(rows) == 1 and len(rows[0]) == 7, "unreadable memory availability")
    return int(rows[0][-1]) / 1024


def shard_counts(suites, memory, mssql=None, pg=None):
    require(math.isfinite(memory) and memory > 0, "available memory must be positive and finite")
    counts = {}
    for suite, requested in (("mssql", mssql), ("pg", pg)):
        if suite in suites and requested is not None:
            require(requested > 0, "shard count must be positive")
            require(suite != "mssql" or requested <= 4, "at most four SQL Server shards are supported")
            counts[suite] = requested
    if "pg" in suites and "pg" not in counts:
        reserve = mssql_memory(counts.get("mssql", 1)) if "mssql" in suites else 0
        counts["pg"] = max(1, min(4, int((memory - reserve) // PG_SHARD_GIB)))
    if "mssql" in suites and "mssql" not in counts:
        remaining = memory - PG_SHARD_GIB * counts.get("pg", 0)
        counts["mssql"] = max(1, min(4, 1 + int((remaining - MSSQL_SET_GIB) // MSSQL_WITHOUT_EXPRESS_GIB)))
    required = mssql_memory(counts.get("mssql", 0)) + PG_SHARD_GIB * counts.get("pg", 0)
    require(required <= memory,
            f"shards need about {required} GiB; only {memory:.2f} GiB is available")
    return counts


def mssql_memory(count):
    return MSSQL_SET_GIB + (count - 1) * MSSQL_WITHOUT_EXPRESS_GIB if count else 0


def sample_memory(stop, samples, errors):
    while not stop.wait(MEMORY_SAMPLE_SECONDS):
        try:
            samples.append(available_gb())
        except (ValueError, OSError, subprocess.SubprocessError) as error:
            errors.append(str(error))
            return


def drive(args):
    suites = list(RUNNERS) if args.suite == "all" else [args.suite]
    memory = available_gb()
    counts = shard_counts(suites, memory, args.mssql_shards, args.pg_shards)
    print(f"available memory: {memory:.2f} GiB; shards: "
          + ", ".join(f"{suite}={counts[suite]}" for suite in suites), flush=True)
    env = dict(os.environ, PBPS_LIVE_PREFIX=args.prefix)
    # Identity and resume evidence are settled before anything is created, so a
    # refused resume leaves no output directory behind (DEC-2553.1).
    tree = tree_identity(ROOT)
    try:
        inputs, inputs_env = inputs_digest(env)
    except (ValueError, OSError):
        inputs, inputs_env = None, []
    run_id = uuid.uuid4().hex
    previous = getattr(args, "resume", None)
    proven, failures, waived, origin = {}, {}, [], None
    if previous:
        require(args.output and Path(previous).resolve() != Path(args.output).resolve(),
                "--resume needs a fresh --output of its own")
        proven, failures, waived, origin = load_resume(
            previous, tree, inputs, suites, register_flakes(getattr(args, "flakes_ref", "origin/master")))
    output = Path(args.output or tempfile.mkdtemp(prefix="pbps-live-shards-"))
    output.mkdir(parents=True, exist_ok=True)
    weights = read_weights(args.weights)
    # One immutable snapshot for the whole run prevents different shards from
    # repartitioning as measurements are published by another run.
    snapshot = output / "weights-input.json"
    require(not snapshot.exists(), "driver output directory is not fresh")
    snapshot.write_text(json.dumps(weights))
    if weights is None:
        env.pop("PBPS_LIVE_WEIGHTS", None)
    else:
        env["PBPS_LIVE_WEIGHTS"] = str(snapshot.resolve())
    env["PBPS_LIVE_RUN"] = run_id
    for name, value in (("PBPS_LIVE_TREE", tree), ("PBPS_LIVE_INPUTS", inputs)):
        if value is not None:
            env[name] = value
    resumed_from = None
    if previous:
        snapshot_path = output / "resume-input.json"
        write_atomic(snapshot_path, json.dumps({"run_id": origin["run_id"], "keys": proven}))
        env["PBPS_LIVE_RESUME"] = str(snapshot_path.resolve())
        summary_path = Path(previous) / "summary.json"
        resumed_from = {"run_id": origin["run_id"], "output": str(Path(previous).resolve()),
                        "manifest_sha256": hashlib.sha256((Path(previous) / MANIFEST).read_bytes()).hexdigest(),
                        "summary_sha256": (hashlib.sha256(summary_path.read_bytes()).hexdigest()
                                           if summary_path.exists() else None),
                        "flakes": [{k: f[k] for k in ("test", "cargo", "issue")} for f in waived]}
    write_atomic(output / MANIFEST, json.dumps(
        {"version": 1, "run_id": run_id, "started": datetime.datetime.now(datetime.timezone.utc).isoformat(),
         "suites": suites, "shards": counts, "tree": tree, "inputs": inputs, "inputs_env": inputs_env,
         "failed": failures, "resumed_from": resumed_from}, indent=2) + "\n")
    summary, measured = {}, {}
    used_names, used_ports = set(), set()
    tasks = []
    for suite in suites:
        count = counts[suite]
        suite_root = output / suite
        suite_root.mkdir()
        identities = [fixture_env(suite, i, env) for i in range(count)]
        for slot in (0, 1):
            selected = [settings[fixture[slot]] for settings in identities for fixture in FIXTURES[suite]]
            used = used_names if slot == 0 else used_ports
            require(len(selected) == len(set(selected)) and not used.intersection(selected),
                    "fixture identities overlap across shards or suites")
            used.update(selected)
        tasks.extend((suite, index) for index in range(count))

    express = preflight_express(counts["mssql"], weights, env) if "mssql" in suites else []

    def launch(task):
        suite, index = task
        started = time.monotonic()
        suite_root = output / suite
        shard_dir = (suite_root / str(index)).resolve()
        shard_env = dict(env, PBPS_LIVE_SHARD=f"{index}/{counts[suite]}",
                         PBPS_LIVE_SHARD_DIR=str(shard_dir))
        try:
            with (suite_root / f"{index}.log").open("w") as log:
                code = fixtures.run(["bash", str(ROOT / "scripts" / RUNNERS[suite])], cwd=ROOT,
                                    env=shard_env, stdout=log, stderr=subprocess.STDOUT)
        except OSError as error:
            print(f"{suite}/{index}: {error}", file=sys.stderr, flush=True)
            code = 1
        return suite, index, code, started, time.monotonic()

    started = time.monotonic()
    # Submit every shard before waiting: a slow SQL Server must not delay PG,
    # and one failed suite must not prevent the other's results being audited.
    memory_samples, memory_errors = [memory], []
    stop = Event()
    sampler = Thread(target=sample_memory, args=(stop, memory_samples, memory_errors))
    with FixtureRun(used_names) as fixtures:
        try:
            sampler.start()
            with ThreadPoolExecutor(max_workers=len(tasks)) as pool:
                finished = list(pool.map(launch, tasks))
        finally:
            stop.set()
            if sampler.ident is not None:
                sampler.join()
    failed = bool(memory_errors)
    for suite in suites:
        count = counts[suite]
        suite_root = output / suite
        runs = [run for run in finished if run[0] == suite]
        codes = [run[2] for run in runs]
        try:
            result, timings = audit(suite_root, count)
        except (ValueError, OSError, LookupError, TypeError, AttributeError) as error:
            statuses = []
            for path in suite_root.glob("*/*.json"):
                try:
                    row = json.loads(path.read_text())
                    statuses.extend(r["status"] for r in row["reported"].values())
                except (ValueError, OSError, LookupError, TypeError, AttributeError):
                    pass
            result = {"passed": statuses.count("ok"), "failed": statuses.count("FAILED"),
                      "union": "failed", "error": str(error)}
            timings = {}
        first = min(run[3] for run in runs)
        last = max(run[4] for run in runs)
        result.update(seconds=last - first, shards=count, exits=codes,
                      shard_seconds={str(run[1]): run[4] - run[3] for run in runs},
                      start_seconds=first - started, end_seconds=last - started)
        summary[suite] = result
        measured.update(timings)
        print(f"{suite}: {result['passed']} passed; {result['failed']} failed; union {result['union']}; "
              f"{result['seconds']:.2f}s; logs {suite_root}", flush=True)
        failed |= bool(any(codes) or result["failed"] or result["union"] != "exact")
    elapsed = time.monotonic() - started
    summary["run"] = {"seconds": elapsed, "available_gib": memory, "shards": counts,
                      "minimum_available_gib": min(memory_samples), "memory_samples": len(memory_samples),
                      "memory_sample_seconds": MEMORY_SAMPLE_SECONDS, "memory_errors": memory_errors}
    summary["run"].update(run_id=run_id, tree=tree, inputs=inputs, resumed_from=resumed_from)
    summary["run"]["express_tests_on_shard_zero"] = express
    (output / "summary.json").write_text(json.dumps(summary, indent=2) + "\n")
    print(f"total: {elapsed:.2f}s; minimum available memory: {min(memory_samples):.2f} GiB; "
          f"summary {output / 'summary.json'}", flush=True)
    if args.measure and not failed:
        weights = weights or {}
        weights.update(measured)
        Path(args.measure).write_text(json.dumps(weights, indent=2, sort_keys=True) + "\n")
    return int(failed)


def main():
    if sys.argv[1:2] == ["ci"]:
        parser = argparse.ArgumentParser(description="Shard one ignored cargo test selection")
        parser.add_argument("--expected-tests", type=int)
        parser.add_argument("--allow-empty", action="store_true",
                            help="allow empty invocation shares only with audited output receipts")
        parser.add_argument("--output", help="write enumerated and executed test receipts")
        parser.add_argument("selector")
        parser.add_argument("cargo", nargs=argparse.REMAINDER)
        args = parser.parse_args(sys.argv[2:])
        return ci_shard(args.selector, args.cargo, expected_tests=args.expected_tests,
                        output=args.output, allow_empty=args.allow_empty)
    if sys.argv[1:2] == ["audit"]:
        parser = argparse.ArgumentParser(description="Audit receipts from every shard")
        parser.add_argument("output", type=Path)
        parser.add_argument("count", type=int)
        parser.add_argument("--expected-invocations", type=int)
        args = parser.parse_args(sys.argv[2:])
        require(args.count > 0, "shard count must be positive")
        result, _ = audit(args.output, args.count, expected_invocations=args.expected_invocations)
        print(json.dumps(result, sort_keys=True))
        return int(bool(result["failed"]))
    # Internal entries let the shell runners retain their authoritative command
    # sequence. Unset shard mode never enters this driver or its cargo proxy.
    if sys.argv[1:2] == ["cargo"]:
        return cargo_proxy(sys.argv[2:])
    if sys.argv[1:2] == ["shard"]:
        return shard(sys.argv[2], sys.argv[3:])
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--suite", choices=["all", *RUNNERS], default="all")
    parser.add_argument("--mssql-shards", type=int)
    parser.add_argument("--pg-shards", type=int)
    parser.add_argument("--prefix", default="pbps-live")
    parser.add_argument("--weights")
    parser.add_argument("--measure", help="write observed per-test seconds as a weights JSON file")
    parser.add_argument("--output", help="fresh directory for receipts, per-shard logs and summary")
    parser.add_argument("--resume", metavar="PREVIOUS_OUTPUT",
                        help="finish a stopped run of this same head: rerun only the names it did not prove")
    parser.add_argument("--flakes-ref", default="origin/master",
                        help="git ref whose scripts/known-flakes.json may waive FAILED names on --resume")
    return drive(parser.parse_args())


if __name__ == "__main__":
    try:
        sys.exit(main())
    except (ValueError, OSError, subprocess.SubprocessError) as error:
        print(f"live shards: {error}", file=sys.stderr)
        sys.exit(1)
