#!/usr/bin/env python3
"""Run the repository live runners in isolated shards and audit their receipts."""

import argparse
from collections import Counter
from concurrent.futures import ThreadPoolExecutor
import hashlib
import json
import math
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tempfile
import time


ROOT = Path(__file__).resolve().parents[1]
SCRIPT = Path(__file__).resolve()
RUNNERS = {"mssql": "live-tests.sh", "pg": "live-tests-pg.sh"}
FIXTURES = {
    "mssql": [("PBPS_TEST_MSSQL_NAME", "PBPS_TEST_PORT", "mssql", 26330),
              ("PBPS_TEST_MSSQL_EXPRESS_NAME", "PBPS_TEST_EXPRESS_PORT", "mssql-express", 26331),
              ("PBPS_TEST_MSSQL_SCRATCH_NAME", "PBPS_TEST_MSSQL_SCRATCH_PORT", "mssql-scratch", 26332)],
    "pg": [("PBPS_TEST_PG_NAME", "PBPS_TEST_PG_PORT", "pg", 56320),
           ("PBPS_TEST_PG_OLD_NAME", "PBPS_TEST_PG_OLD_PORT", "pg16", 56321),
           ("PBPS_TEST_PG_SCRATCH_NAME", "PBPS_TEST_PG_SCRATCH_PORT", "pg-scratch", 56322)],
}


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
    for name, port, kind, default in FIXTURES[suite]:
        base = env.get(name)
        env[name] = f"{base}-s{index}" if base else f"{prefix}s{index}-{kind}"
        if name == "PBPS_TEST_MSSQL_SCRATCH_NAME" and not base:
            env[name] = env["PBPS_TEST_MSSQL_NAME"] + "-scratch"
        require(re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9_.-]*", env[name]), "invalid fixture name")
        env[port] = str(int(env.get(port) or default) + 10 * index)
        require(0 < int(env[port]) < 65536, "fixture port outside 1..65535")
    require(len({env[n] for n, _, _, _ in FIXTURES[suite]}) == 3, "fixture names collide")
    require(len({env[p] for _, p, _, _ in FIXTURES[suite]}) == 3, "fixture ports collide")
    return env


def partition(names, count, identity, weights, pinned=False):
    require(len(set(names)) == len(names), "duplicate name in --list output")
    groups = [[] for _ in range(count)]
    if pinned:
        groups[0] = list(names)
    elif weights is not None:
        loads = [0.0] * count
        for name in sorted(names, key=lambda n: (-weights.get(identity + ":" + n, 1.0), n)):
            index = min(range(count), key=lambda i: (loads[i], i))
            groups[index].append(name)
            loads[index] += weights.get(identity + ":" + name, 1.0)
    else:
        for name in names:
            digest = hashlib.sha256((identity + ":" + name).encode()).digest()
            groups[int.from_bytes(digest, "big") % count].append(name)
    verify_union(names, groups)
    return [sorted(group) for group in groups]


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
    groups = partition(names, count, identity, read_weights(env.get("PBPS_LIVE_WEIGHTS")), pinned)
    assigned = groups[index]
    print(f"shard {index}/{count} {key}: {len(assigned)}/{len(names)} tests", flush=True)
    reported, code, seconds, valid = {}, 0, 0.0, True
    if assigned:
        reported, code, seconds, valid = run_tests(
            [real, *cargo, *flags, "--exact", "--test-threads=1", "--format=pretty",
             "--color=never", *assigned], assigned)
    receipt = {"args": args, "identity": identity, "listed": names, "groups": groups,
               "assigned": assigned, "reported": reported, "pinned": pinned, "seconds": seconds,
               "valid": valid}
    (root / (key + ".json")).write_text(json.dumps(receipt, indent=2) + "\n")
    require(valid, "assigned tests or their libtest summary were not reported exactly once")
    return code


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
    env.update(PBPS_LIVE_ACTIVE="1", PBPS_LIVE_REAL_CARGO=real,
               PBPS_LIVE_SHARD_DIR=str(root), PATH=str(proxy.parent) + os.pathsep + env["PATH"])
    result = subprocess.run(["bash", str(ROOT / "scripts" / RUNNERS[suite]), *extra], cwd=ROOT, env=env)
    require(any(json.loads(p.read_text())["assigned"] for p in root.glob("*.json")),
            "shard is empty for the whole suite")
    return result.returncode


def audit(suite_root, count):
    receipts = [{p.stem: json.loads(p.read_text()) for p in (suite_root / str(i)).glob("*.json")}
                for i in range(count)]
    require(all(any(r["assigned"] for r in shard.values()) for shard in receipts),
            "a shard is empty for the whole suite")
    passed = failed = 0
    measured = {}
    for key in sorted(set().union(*(set(shard) for shard in receipts))):
        rows = [shard.get(key) for shard in receipts]
        reference = next(row for row in rows if row is not None)
        require(all(row is not None for row in rows) or
                reference["pinned"] and rows[0] is not None
                and all(row is None for row in rows[1:]), "missing invocation receipt")
        groups = []
        for index, row in enumerate(rows):
            if row is None:
                groups.append([])
                continue
            require(all(row[k] == reference[k] for k in ("args", "identity", "listed", "groups", "pinned")),
                    "shards disagree about the invocation or partition")
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
        verify_union(reference["listed"], groups)
    return {"passed": passed, "failed": failed, "union": "exact"}, measured


def available_gb():
    # Linux's MemAvailable includes reclaimable cache; MemFree does not.
    output = subprocess.check_output(["free", "-m"], text=True)
    return int(next(line.split()[-1] for line in output.splitlines() if line.startswith("Mem:"))) / 1024


def drive(args):
    output = Path(args.output or tempfile.mkdtemp(prefix="pbps-live-shards-"))
    output.mkdir(parents=True, exist_ok=True)
    env = dict(os.environ, PBPS_LIVE_PREFIX=args.prefix)
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
    summary, measured = {}, {}
    used_names, used_ports = set(), set()
    for suite in RUNNERS if args.suite == "all" else (args.suite,):
        count = getattr(args, suite + "_shards")
        if count is None:
            count = max(1, min(int(available_gb() // (12 if suite == "mssql" else 1)),
                               2 if suite == "mssql" else 6))
        require(count > 0, "shard count must be positive")
        suite_root = output / suite
        suite_root.mkdir()
        identities = [fixture_env(suite, i, env) for i in range(count)]
        for slot in (0, 1):
            selected = [settings[fixture[slot]] for settings in identities for fixture in FIXTURES[suite]]
            used = used_names if slot == 0 else used_ports
            require(len(selected) == len(set(selected)) and not used.intersection(selected),
                    "fixture identities overlap across shards or suites")
            used.update(selected)
        started = time.monotonic()

        def launch(index):
            shard_dir = (suite_root / str(index)).resolve()
            shard_env = dict(env, PBPS_LIVE_SHARD=f"{index}/{count}", PBPS_LIVE_SHARD_DIR=str(shard_dir))
            with (suite_root / f"{index}.log").open("w") as log:
                return subprocess.run(["bash", str(ROOT / "scripts" / RUNNERS[suite])], cwd=ROOT,
                                      env=shard_env, stdout=log, stderr=subprocess.STDOUT).returncode

        with ThreadPoolExecutor(max_workers=count) as pool:
            codes = list(pool.map(launch, range(count)))
        try:
            result, timings = audit(suite_root, count)
        except ValueError as error:
            rows = [json.loads(p.read_text()) for p in suite_root.glob("*/*.json")]
            statuses = [r["status"] for row in rows for r in row["reported"].values()]
            result = {"passed": statuses.count("ok"), "failed": statuses.count("FAILED"),
                      "union": "failed", "error": str(error)}
            timings = {}
        result.update(seconds=time.monotonic() - started, shards=count, exits=codes)
        summary[suite] = result
        measured.update(timings)
        (output / "summary.json").write_text(json.dumps(summary, indent=2) + "\n")
        print(f"{suite}: {result['passed']} passed; {result['failed']} failed; union {result['union']}; "
              f"{result['seconds']:.2f}s; logs {suite_root}", flush=True)
        require(not any(codes) and not result["failed"] and result["union"] == "exact",
                f"{suite} shards failed; inspect {suite_root}")
    if args.measure:
        weights = weights or {}
        weights.update(measured)
        Path(args.measure).write_text(json.dumps(weights, indent=2, sort_keys=True) + "\n")
    return 0


def main():
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
    return drive(parser.parse_args())


if __name__ == "__main__":
    try:
        sys.exit(main())
    except (ValueError, OSError, subprocess.SubprocessError) as error:
        print(f"live shards: {error}", file=sys.stderr)
        sys.exit(1)
