#!/usr/bin/env python3
"""Regressions for live-test execution ownership, without starting fixtures."""

import argparse
from contextlib import redirect_stdout
import importlib.util
import io
import json
import os
from pathlib import Path
import re
import shlex
import signal
import subprocess
import sys
import tempfile
import textwrap
import time
import unittest
from unittest import mock


ROOT = Path(__file__).resolve().parents[1]
_spec = importlib.util.spec_from_file_location("live_shards", ROOT / "scripts/live-shards.py")
SHARDS = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(SHARDS)
FIXTURE_NAMES = {
    "PBPS_TEST_MSSQL_NAME": "pbps-test-mssql",
    "PBPS_TEST_MSSQL_EXPRESS_NAME": "pbps-test-mssql-express",
    "PBPS_TEST_MSSQL_SCRATCH_NAME": "pbps-test-mssql-scratch",
    "PBPS_TEST_PG_NAME": "pbps-test-pg",
    "PBPS_TEST_PG_OLD_NAME": "pbps-test-pg16",
    "PBPS_TEST_PG_SCRATCH_NAME": "pbps-test-pg-scratch",
}


def fixture_environment(**overrides):
    # Host-local lane settings must not turn default-name regressions into
    # custom-name runs or silently change the endpoints asserted below.
    env = {key: value for key, value in os.environ.items()
           if key != "NAME" and not key.startswith(("PBPS_TEST_", "PBPS_LIVE_"))}
    env.update(overrides)
    return env


def job_commands(workflow, job):
    # These invocations are single-line run steps. A comment or the same
    # command in another job cannot supply this engine's live connection.
    jobs = list(re.finditer(r"^  ([\w-]+):\s*$", workflow, re.MULTILINE))
    for index, match in enumerate(jobs):
        if match[1] == job:
            end = jobs[index + 1].start() if index + 1 < len(jobs) else len(workflow)
            body = workflow[match.end():end]
            return [shlex.split(command) for command in
                    re.findall(r"^      - run: (.+)$", body, re.MULTILINE)]
    return []


class ShardedExecution(unittest.TestCase):
    def test_descendant_cleanup_finishes_after_its_runner_leader_exits(self):
        events, elapsed, _ = self.stop_fake_runners(["cleanup"])
        kinds = [event[0] for event in events]
        self.assertIn("cleanup", kinds, "SIGKILL must not bypass the helper's finally block")
        self.assertLess(kinds.index("leader-exit"), kinds.index("cleanup"))
        self.assertNotIn("kill", kinds)
        self.assertLess(elapsed, 0.2)

    def test_ignoring_descendant_is_killed_at_the_deadline_after_its_leader_exits(self):
        events, elapsed, fixture = self.stop_fake_runners(["ignore"])
        self.assertEqual(fixture.processes, [], "the leader was already reaped")
        self.assertEqual([event[0] for event in events], ["term", "wait", "leader-exit", "kill"])
        self.assertAlmostEqual(elapsed, 0.2)
        self.assertAlmostEqual(events[-1][2], 0.2)

    def test_runner_shutdown_uses_one_deadline_and_releases_the_registry_lock(self):
        events, elapsed, _ = self.stop_fake_runners(["blocked", "blocked"])
        self.assertEqual([event[0] for event in events], ["term", "term", "wait", "wait", "kill", "kill"])
        waits = [event[2] for event in events if event[0] == "wait"]
        self.assertEqual(waits, [0.2, 0])
        self.assertAlmostEqual(elapsed, 0.2)

    def test_already_gone_runner_groups_need_no_escalation(self):
        events, elapsed, _ = self.stop_fake_runners(["gone"])
        self.assertNotIn("kill", [event[0] for event in events])
        self.assertEqual(elapsed, 0)

    def stop_fake_runners(self, behaviors):
        fixture = SHARDS.FixtureRun([])
        clock, events = [0.0], []
        groups = {1000 + index: {"behavior": behavior, "alive": behavior != "gone", "term": None}
                  for index, behavior in enumerate(behaviors)}

        def send(pid, sig):
            group = groups[pid]
            if sig == signal.SIGTERM:
                self.assertTrue(fixture.lock.acquire(blocking=False))
                fixture.lock.release()
                events.append(("term", pid, clock[0]))
                group["term"] = clock[0]
            elif sig == signal.SIGKILL:
                events.append(("kill", pid, clock[0]))
                group["alive"] = False
                return
            elif group["behavior"] == "cleanup" and group["term"] is not None:
                if clock[0] >= group["term"] + 0.05:
                    events.append(("cleanup", pid, clock[0]))
                    group["alive"] = False
            if not group["alive"]:
                raise ProcessLookupError(pid)

        def wait(process, timeout):
            # The real worker removes its leader under this lock after wait().
            # Holding the registry lock throughout shutdown would deadlock it.
            self.assertTrue(fixture.lock.acquire(blocking=False))
            try:
                self.assertTrue(fixture.stopped)
                events.append(("wait", process.pid, timeout))
                if groups[process.pid]["behavior"] != "blocked":
                    fixture.processes.remove(process)
                    events.append(("leader-exit", process.pid, clock[0]))
                    return 0
            finally:
                fixture.lock.release()
            clock[0] += timeout
            raise subprocess.TimeoutExpired("fake runner", timeout)

        for pid in groups:
            process = mock.Mock(pid=pid)
            process.wait.side_effect = lambda timeout, process=process: wait(process, timeout)
            fixture.processes.append(process)

        def sleep(seconds):
            self.assertGreater(seconds, 0)
            clock[0] += seconds

        with mock.patch.object(SHARDS, "RUNNER_STOP_GRACE_SECONDS", 0.2, create=True), \
             mock.patch.object(SHARDS.os, "killpg", side_effect=send), \
             mock.patch.object(SHARDS.time, "monotonic", side_effect=lambda: clock[0]), \
             mock.patch.object(SHARDS.time, "sleep", side_effect=sleep):
            fixture.stop()
        return events, clock[0], fixture

    def test_default_ports_avoid_ephemeral_and_canonical_runner_ports(self):
        ports = []
        with mock.patch.object(SHARDS, "ephemeral_ports", return_value=(32768, 60999)):
            for suite, count in (("mssql", 4), ("pg", 4)):
                for index in range(count):
                    env = SHARDS.fixture_env(suite, index, {})
                    ports.extend(int(env[port]) for _, port, _, _ in SHARDS.FIXTURES[suite])
        self.assertEqual(len(ports), len(set(ports)))
        self.assertTrue(all(port < 32768 for port in ports))
        self.assertTrue(set(ports).isdisjoint({14330, 14331, 14332, 54320, 54321, 54322}))

    def test_ephemeral_overlap_names_each_fixture_and_refuses_before_launch(self):
        for suite in SHARDS.RUNNERS:
            for name, port, _, _ in SHARDS.FIXTURES[suite]:
                for base in (39990, 40000):
                    with self.subTest(suite=suite, port=port, base=base), \
                         mock.patch.object(SHARDS, "ephemeral_ports", return_value=(40000, 40010)):
                        with self.assertRaisesRegex(ValueError, f"owned-s1.*{port}=400[01]0"):
                            SHARDS.fixture_env(suite, 1, {name: "owned", port: str(base)})
        with mock.patch.object(SHARDS, "ephemeral_ports", return_value=(27320, 27330)):
            with self.assertRaisesRegex(ValueError, "overlaps host ephemeral range"):
                self.overlap_run()
        with mock.patch.object(SHARDS, "ephemeral_ports", return_value=(40000, 40010)):
            self.assertEqual(SHARDS.fixture_env("pg", 0, {"PBPS_TEST_PG_PORT": "39999"})
                             ["PBPS_TEST_PG_PORT"], "39999")
            self.assertEqual(SHARDS.fixture_env("pg", 0, {"PBPS_TEST_PG_PORT": "40011"})
                             ["PBPS_TEST_PG_PORT"], "40011")

    def test_unreadable_or_malformed_ephemeral_range_is_a_refusal(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "range"
            with mock.patch.object(SHARDS, "EPHEMERAL_RANGE", path):
                with self.assertRaisesRegex(ValueError, "cannot read host ephemeral port range"):
                    self.overlap_run()
                for text in ("", "32768", "low high", "32768 60999 extra", "0 65535", "40000 30000"):
                    path.write_text(text)
                    with self.subTest(text=text), self.assertRaisesRegex(ValueError, "ephemeral port range"):
                        SHARDS.fixture_env("pg", 0, {})
                path.write_text("32768\t60999\n")
                self.assertEqual(SHARDS.ephemeral_ports(), (32768, 60999))
                with mock.patch.object(Path, "read_text", side_effect=PermissionError("denied")):
                    with self.assertRaisesRegex(ValueError, "cannot read host ephemeral port range"):
                        SHARDS.fixture_env("pg", 0, {})

    def test_cleanup_removes_exact_owned_names_and_volumes_on_success_failure_and_failed_bind(self):
        foreign = ["pbps-test-shards0-pg-foreign", "pbps-test-shards0-pg16.extra"]
        for mode in ("ok", "failed", "bind-failed"):
            with self.subTest(mode=mode):
                remaining = []
                result, calls, _, _ = self.fake_run("pg", 1, mode, existing=foreign, remaining=remaining)
                self.assertEqual(result.returncode == 0, mode == "ok", result.stderr)
                created = [r["command"][r["command"].index("--name") + 1] for r in calls
                           if r["command"][:2] == ["docker", "run"]]
                removed = [r["command"][2:] for r in calls if r["command"][:2] == ["docker", "rm"]]
                self.assertTrue(created)
                self.assertCountEqual(removed, [["-f", "-v", name] for name in created])
                self.assertCountEqual(remaining, foreign)

    def test_cleanup_stops_runner_descendants_on_sigint_and_sigterm(self):
        for sig in (signal.SIGINT, signal.SIGTERM):
            with self.subTest(signal=sig):
                remaining = []
                result, calls, _, _ = self.fake_run("pg", 2, "interrupt", interrupt=sig,
                                                  remaining=remaining)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn(signal.Signals(sig).name, result.stderr)
                self.assertEqual(remaining, [])
                created = [r["command"][r["command"].index("--name") + 1] for r in calls
                           if r["command"][:2] == ["docker", "run"]]
                removed = [r["command"][-1] for r in calls if r["command"][:2] == ["docker", "rm"]]
                self.assertTrue(created)
                self.assertCountEqual(removed, created)

    def test_interrupt_during_sampler_start_stops_the_started_thread(self):
        for create_thread in (False, True):
            with self.subTest(create_thread=create_thread), tempfile.TemporaryDirectory() as directory:
                self.check_sampler_start_interruption(directory, create_thread)

    def check_sampler_start_interruption(self, directory, create_thread):
        args = argparse.Namespace(suite="pg", mssql_shards=None, pg_shards=1,
                                  prefix="sampler", weights=None, measure=None, output=directory)
        started = []
        original_start = SHARDS.Thread.start

        def interrupt_start(thread):
            started.append((thread, thread._args[0]))
            if create_thread:
                original_start(thread)
            # A signal may arrive after start() creates the thread but
            # before the caller executes its next statement.
            raise InterruptedError("interrupted during sampler start")

        try:
            with mock.patch.object(SHARDS, "available_gb", return_value=32), \
                 mock.patch.object(SHARDS, "container_names", return_value=set()), \
                 mock.patch.object(SHARDS.Thread, "start", interrupt_start), \
                 redirect_stdout(io.StringIO()):
                with self.assertRaisesRegex(InterruptedError, "sampler start"):
                    SHARDS.drive(args)
            self.assertTrue(started)
            self.assertFalse(started[0][0].is_alive(), "interruption must not strand a non-daemon sampler")
        finally:
            for thread, stop in started:
                stop.set()
                if thread.ident is not None:
                    thread.join()

    def test_consecutive_prefixes_release_the_same_ports_for_the_next_run(self):
        remaining = []
        result, calls, summary, _ = self.fake_run("pg", 1, remaining=remaining, repeat_prefix="next-shard")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(summary["pg"]["union"], "exact")
        self.assertEqual(remaining, [])
        created = [r["command"][r["command"].index("--name") + 1] for r in calls
                   if r["command"][:2] == ["docker", "run"]]
        self.assertEqual(len(created), 6)
        self.assertTrue(any(name.startswith("pbps-test-shard") for name in created))
        self.assertTrue(any(name.startswith("next-shard") for name in created))

    def test_cleanup_failure_is_reported_and_other_owned_containers_are_still_removed(self):
        remaining = []
        result, calls, _, _ = self.fake_run("pg", 1, "cleanup-failed", remaining=remaining)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("fixture cleanup failed", result.stderr)
        self.assertIn("pbps-test-shards0-pg: removal denied", result.stderr)
        self.assertEqual(remaining, ["pbps-test-shards0-pg"])
        self.assertEqual(sum(r["command"][:2] == ["docker", "rm"] for r in calls), 3)

    def test_existing_assignment_and_unreadable_container_listing_are_never_owned(self):
        existing = ["pbps-test-shards0-pg"]
        remaining = []
        result, calls, _, _ = self.fake_run("pg", 1, existing=existing, remaining=remaining)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("fixture containers already exist", result.stderr)
        self.assertEqual(remaining, existing)
        self.assertFalse(any(r["command"][:2] in (["docker", "run"], ["docker", "rm"]) for r in calls))
        for mode in ("list-failed", "cleanup-list-failed"):
            with self.subTest(mode=mode):
                result, calls, _, _ = self.fake_run("pg", 1, mode)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("cannot list fixture containers", result.stderr)
                self.assertFalse(any(r["command"][:2] == ["docker", "rm"] for r in calls))

    def test_available_memory_requires_readable_memavailable(self):
        with mock.patch.object(SHARDS.subprocess, "check_output", return_value="Mem: 4096 1 2 3 4 3072\n"):
            self.assertEqual(SHARDS.available_gb(), 3)
        for output in ("", "Mem: 4096 1\n", "Mem: 1 2 3 4 5 ?\n",
                       "Mem: 1 2 3 4 5 6\nMem: 1 2 3 4 5 6\n"):
            with mock.patch.object(SHARDS.subprocess, "check_output", return_value=output):
                with self.assertRaises(ValueError):
                    SHARDS.available_gb()

    def test_joint_defaults_reserve_postgresql_before_sql_server(self):
        for memory, expected in ((64, {"mssql": 4, "pg": 4}),
                                 (25, {"mssql": 4, "pg": 4}),
                                 (24.96, {"mssql": 3, "pg": 4}),
                                 (23, {"mssql": 3, "pg": 4}),
                                 (22, {"mssql": 3, "pg": 4}),
                                 (21.99, {"mssql": 3, "pg": 4}),
                                 (21, {"mssql": 3, "pg": 4}),
                                 (20.99, {"mssql": 2, "pg": 4}),
                                 (19, {"mssql": 2, "pg": 4}),
                                 (18, {"mssql": 2, "pg": 4}),
                                 (17, {"mssql": 2, "pg": 4}),
                                 (16.99, {"mssql": 1, "pg": 4}),
                                 (16, {"mssql": 1, "pg": 4}),
                                 (15.99, {"mssql": 1, "pg": 4}),
                                 (13, {"mssql": 1, "pg": 4}),
                                 (11, {"mssql": 1, "pg": 2})):
            with self.subTest(memory=memory):
                counts = SHARDS.shard_counts(["mssql", "pg"], memory)
                self.assertEqual(counts, expected)
                self.assertLessEqual(SHARDS.mssql_memory(counts["mssql"]) + counts["pg"], memory)
        self.assertEqual(SHARDS.shard_counts(["pg"], 20), {"pg": 4})
        self.assertEqual(SHARDS.shard_counts(["mssql"], 24), {"mssql": 4})
        self.assertEqual(SHARDS.shard_counts(["mssql", "pg"], 25, mssql=4, pg=4), {"mssql": 4, "pg": 4})
        self.assertEqual(SHARDS.shard_counts(["mssql"], 21, mssql=4), {"mssql": 4})
        for memory, overrides in ((9, {}), (16.99, {"mssql": 2, "pg": 4}),
                                  (24.99, {"mssql": 4, "pg": 4}),
                                  (21.99, {"mssql": 4, "pg": None}),
                                  (64, {"mssql": 5}),
                                  (32, {"pg": 0}), (32, {"mssql": -1})):
            with self.assertRaises(ValueError):
                SHARDS.shard_counts(["mssql", "pg"], memory, **overrides)

    def test_express_names_are_pinned_and_weights_balance_around_their_load(self):
        express = sorted(SHARDS.EXPRESS_TESTS)
        names = express + ["a", "b", "c"]
        weights = {"bin:" + name: 10 for name in express}
        weights.update({"bin:a": 20, "bin:b": 15, "bin:c": 5})
        groups = SHARDS.partition(names, 4, "bin", weights)
        self.assertEqual(groups, [express, ["a"], ["b"], ["c"]])
        SHARDS.verify_express(groups)
        SHARDS.verify_express(SHARDS.partition(names, 4, "bin", None))
        with self.assertRaisesRegex(ValueError, "Express"):
            SHARDS.verify_express([[], express, ["a", "b", "c"]])

    def test_express_classification_fails_before_launch_and_nonzero_shards_start_none(self):
        result, calls, _, _ = self.fake_run("mssql", 4, "express-missing")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("classified exact name", result.stderr)
        self.assertFalse(any(row["command"][0] == "docker" for row in calls))
        result, calls, summary, receipts = self.fake_run("mssql", 4)
        self.assertEqual(result.returncode, 0, result.stderr)
        started = [row for row in calls if row["command"][:2] == ["docker", "run"]
                   and "MSSQL_PID=Express" in row["command"]]
        self.assertEqual([row["environment"]["PBPS_LIVE_SHARD"] for row in started], ["0/4"])
        fourth = [row["command"] for row in calls if row["command"][:2] == ["docker", "run"]
                  and row["environment"]["PBPS_LIVE_SHARD"] == "3/4"]
        self.assertCountEqual([row[row.index("-p") + 1] for row in fourth],
                              ["26360:1433", "127.0.0.1:26362:1433"])
        for row in receipts:
            SHARDS.verify_express(row["groups"])
        self.assertEqual(summary["mssql"]["union"], "exact")

    def test_preflight_refuses_an_express_assignment_on_a_nonzero_shard(self):
        def listing(command, **kwargs):
            cargo = tuple(command[1:command.index("--") + 1])
            names = sorted(SHARDS.EXPRESS_INVENTORY[cargo])
            output = "\n".join(name + ": test" for name in names)
            return subprocess.CompletedProcess(command, 0, output + f"\n{len(names)} tests, 0 benchmarks\n")

        with mock.patch.object(SHARDS.subprocess, "run", side_effect=listing) as listed, \
             mock.patch.object(SHARDS, "partition", return_value=[[], sorted(SHARDS.EXPRESS_TESTS)]):
            with self.assertRaisesRegex(ValueError, "Express-needing test assigned outside shard zero"):
                SHARDS.preflight_express(2, {}, os.environ)
            self.assertEqual(listed.call_count, 1, "refuse during enumeration before any runner starts")

    def test_memory_sampling_keeps_lows_and_reports_unreadable_samples(self):
        for values, expected, errors in (([21, 13, 20], [32, 21, 13, 20], []),
                                         ([OSError("free failed")], [32], ["free failed"])):
            with self.subTest(values=values):
                stop = mock.Mock()
                stop.wait.side_effect = [False] * len(values) + [True]
                samples, observed_errors = [32], []
                with mock.patch.object(SHARDS, "available_gb", side_effect=values):
                    SHARDS.sample_memory(stop, samples, observed_errors)
                self.assertEqual(samples, expected)
                self.assertEqual(observed_errors, errors)
                self.assertTrue(all(call == mock.call(15) for call in stop.wait.call_args_list))

    def test_summary_records_memory_minimum_and_shard_wall_times_even_on_failure(self):
        for errors in ((), ("free failed",)):
            with self.subTest(errors=errors):
                code, summary, _, _ = self.overlap_run(memory_values=(21, 12), memory_errors=errors)
                self.assertEqual(code, int(bool(errors)))
                self.assertEqual(summary["run"]["minimum_available_gib"], 12)
                self.assertEqual(summary["run"]["memory_samples"], 3)
                self.assertEqual(summary["run"]["memory_sample_seconds"], 15)
                self.assertEqual(summary["run"]["memory_errors"], list(errors))
                for suite in SHARDS.RUNNERS:
                    self.assertEqual(summary[suite]["shard_seconds"], {"0": summary[suite]["seconds"]})
                    self.assertEqual(summary[suite]["union"], "exact")

    def test_unknown_weights_use_the_observed_median(self):
        groups = SHARDS.partition(list("abcdef"), 2, "bin", {"bin:a": 100, "bin:b": 10})
        self.assertEqual(groups, [["a", "b", "e"], ["c", "d", "f"]])

    def test_all_suites_overlap_and_a_serial_pool_fails_the_runner_barrier(self):
        for serial in (False, True):
            with self.subTest(serial=serial):
                code, summary, events, requested = self.overlap_run(serial=serial)
                self.assertEqual(requested, [2], "one pool must contain both suites")
                starts = [events[suite]["start"] for suite in SHARDS.RUNNERS]
                ends = [events[suite]["end"] for suite in SHARDS.RUNNERS]
                self.assertEqual(max(starts) < min(ends), not serial)
                self.assertEqual(code, 1 if serial else 0)
                self.assertEqual(set(summary) - {"run"}, set(SHARDS.RUNNERS))
                for suite in SHARDS.RUNNERS:
                    self.assertEqual(summary[suite]["failed"], 1 if serial else 0)

    def test_both_suites_are_audited_when_one_runner_fails(self):
        for failing in ("mssql", "malformed"):
            with self.subTest(failing=failing):
                code, summary, _, _ = self.overlap_run(failing=failing)
                self.assertEqual(code, 1)
                if failing == "mssql":
                    self.assertEqual(summary["mssql"]["failed"], 1)
                else:
                    self.assertEqual(summary["mssql"]["union"], "failed")
                self.assertEqual(summary["pg"]["passed"], 1)
                self.assertEqual(summary["pg"]["union"], "exact")

    def test_cross_suite_port_conflicts_refuse_before_either_runner_starts(self):
        with self.assertRaisesRegex(ValueError, "overlap"):
            self.overlap_run(overrides={"PBPS_TEST_PG_PORT": "26330"})

    def overlap_run(self, serial=False, failing="", overrides=None, memory_values=(), memory_errors=()):
        # The runners rendezvous only while both processes are alive. A single
        # worker cannot satisfy it, regardless of machine speed or test duration.
        with tempfile.TemporaryDirectory(prefix="pbps-suite-overlap-") as directory:
            root = Path(directory)
            (root / "scripts").mkdir()
            runner = root / "runner.py"
            runner.write_text(r'''
import json, os, sys, time
from pathlib import Path
root = Path(os.environ["PBPS_OVERLAP_ROOT"])
suite = sys.argv[1]
peer = "pg" if suite == "mssql" else "mssql"
started = time.monotonic()
active = root / (suite + ".active")
active.touch()
deadline = started + 1.5
while not (root / (peer + ".active")).exists() and time.monotonic() < deadline:
    time.sleep(0.01)
if (root / (peer + ".active")).exists():
    (root / (suite + ".seen")).touch()
while not (root / (peer + ".seen")).exists() and time.monotonic() < deadline:
    time.sleep(0.01)
overlap = (root / (suite + ".seen")).exists() and (root / (peer + ".seen")).exists()
failed = not overlap or os.environ["PBPS_OVERLAP_FAIL"] == suite
row = {"args": ["test"], "identity": suite, "listed": ["case"], "groups": [["case"]],
       "assigned": ["case"], "reported": {"case": {"status": "FAILED" if failed else "ok", "seconds": 1}},
       "pinned": False, "valid": True}
if suite == "mssql" and os.environ["PBPS_OVERLAP_FAIL"] == "malformed":
    row["groups"] = []
output = Path(os.environ["PBPS_LIVE_SHARD_DIR"])
output.mkdir(parents=True)
(output / "case.json").write_text(json.dumps(row))
(root / (suite + ".json")).write_text(json.dumps({"start": started, "end": time.monotonic()}))
active.unlink()
sys.exit(1 if failed else 0)
''')
            for suite, filename in SHARDS.RUNNERS.items():
                (root / "scripts" / filename).write_text(
                    "exec " + shlex.join([sys.executable, str(runner), suite]) + "\n")
            env = fixture_environment(PBPS_OVERLAP_ROOT=str(root), PBPS_OVERLAP_FAIL=failing)
            env.update(overrides or {})
            args = argparse.Namespace(suite="all", mssql_shards=1, pg_shards=1, prefix="overlap",
                                      weights=None, measure=None, output=str(root / "output"))
            executor = SHARDS.ThreadPoolExecutor
            requested = []

            def pool(max_workers):
                requested.append(max_workers)
                return executor(max_workers=1 if serial else max_workers)

            def sample(stop, samples, errors):
                samples.extend(memory_values)
                errors.extend(memory_errors)

            with mock.patch.object(SHARDS, "ROOT", root), \
                 mock.patch.object(SHARDS, "available_gb", return_value=32) as memory, \
                 mock.patch.object(SHARDS, "ThreadPoolExecutor", side_effect=pool), \
                 mock.patch.object(SHARDS, "sample_memory", side_effect=sample), \
                 mock.patch.object(SHARDS, "container_names", return_value=set()), \
                 mock.patch.object(SHARDS, "preflight_express", return_value=sorted(SHARDS.EXPRESS_TESTS)), \
                 mock.patch.dict(os.environ, env, clear=True), redirect_stdout(io.StringIO()):
                try:
                    code = SHARDS.drive(args)
                except ValueError:
                    self.assertFalse(list(root.glob("*.active")))
                    self.assertFalse(list(root.glob("*.seen")))
                    self.assertFalse(list(root.glob("mssql.json")))
                    self.assertFalse(list(root.glob("pg.json")))
                    raise
                self.assertEqual(memory.call_count, 1)
            summary = json.loads((root / "output/summary.json").read_text())
            events = {suite: json.loads((root / (suite + ".json")).read_text()) for suite in SHARDS.RUNNERS}
            return code, summary, events, requested

    def test_partition_is_deterministic_complete_and_rejects_missing_or_duplicate_names(self):
        names = [f"case_{i}" for i in range(40)]
        for weights in (None, {}, {"binary:case_0": 100.0, "binary:case_1": 50.0}):
            with self.subTest(weighted=bool(weights)):
                groups = SHARDS.partition(names, 3, "binary", weights)
                self.assertEqual(groups, SHARDS.partition(list(reversed(names)), 3, "binary", weights))
                SHARDS.verify_union(names, groups)
                dropped = json.loads(json.dumps(groups))
                next(group for group in dropped if group).pop()
                with self.assertRaisesRegex(ValueError, "union"):
                    SHARDS.verify_union(names, dropped)
                with self.assertRaisesRegex(ValueError, "union"):
                    SHARDS.verify_union(names, [*groups, [names[0]]])
        with self.assertRaisesRegex(ValueError, "duplicate"):
            SHARDS.partition(["same", "same"], 2, "binary", {})

    def test_weighted_balance_puts_the_long_poles_on_different_shards(self):
        groups = SHARDS.partition(["a", "b", "c"], 2, "bin", {"bin:a": 10, "bin:b": 9})
        self.assertEqual(groups, [["a"], ["b", "c"]])
        self.assertEqual(SHARDS.partition(["a", "b", "c"], 2, "bin", {}), [["a", "c"], ["b"]])

    def test_shard_identities_are_disjoint_and_invalid_or_colliding_values_refuse(self):
        for suite in SHARDS.RUNNERS:
            first = SHARDS.fixture_env(suite, 0, {})
            second = SHARDS.fixture_env(suite, 1, {})
            for name, port, _, _ in SHARDS.FIXTURES[suite]:
                self.assertNotEqual(first[name], second[name])
                self.assertEqual(int(second[port]) - int(first[port]), 10)
            name_a, port_a, *_ = SHARDS.FIXTURES[suite][0]
            name_b, port_b, *_ = SHARDS.FIXTURES[suite][1]
            for overrides in ({name_a: "bad name"}, {name_a: "same", name_b: "same"},
                              {port_a: "65536"}, {port_a: "12345", port_b: "12345"}):
                with self.subTest(suite=suite, overrides=overrides):
                    with self.assertRaises((ValueError, TypeError)):
                        SHARDS.fixture_env(suite, 0, overrides)
        for value in ("", "1/1", "-1/2", "0/0", "0/two"):
            with self.assertRaises(ValueError):
                SHARDS.selector(value)

    def test_one_shard_preserves_the_canonical_invocation_sequence(self):
        for suite in SHARDS.RUNNERS:
            with self.subTest(suite=suite):
                plain, baseline, _, _ = self.fake_run(suite, None)
                result, calls, summary, receipts = self.fake_run(suite, 1)
                self.assertEqual(plain.returncode, 0, plain.stderr)
                self.assertEqual(result.returncode, 0, result.stderr)
                base = [r["command"][1:] for r in baseline if r["command"][0] == "cargo"]
                executed = [r["command"][1:] for r in calls if r["command"][0] == "cargo"
                            and "--list" not in r["command"]]
                self.assertEqual([c[:c.index("--")] for c in base],
                                 [c[:c.index("--")] for c in executed])
                self.assertCountEqual([r["args"] for r in receipts], base)
                self.assertEqual(summary[suite]["union"], "exact")
                self.assertTrue(all("--exact" in c and "--test-threads=1" in c for c in executed))

    def test_parallel_shards_skip_empty_invocations_and_execute_each_assignment_once(self):
        result, calls, summary, receipts = self.fake_run("pg", 2)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(summary["pg"]["union"], "exact")
        self.assertGreater(summary["pg"]["passed"], 0)
        self.assertEqual(summary["pg"]["failed"], 0)
        empty = [r for r in receipts if not r["assigned"]]
        self.assertTrue(empty, "the one-test type_slots invocation leaves one assignment empty")
        self.assertTrue(all(not r["reported"] for r in empty))
        runs = [r for r in calls if r["command"][0] == "cargo" and "--list" not in r["command"]]
        # Each nonempty invocation runs once; an empty filter must never reach
        # cargo because libtest would interpret it as selecting the whole binary.
        self.assertEqual(len(runs), sum(bool(r["assigned"]) for r in receipts))
        for record in runs:
            self.assertIn("--exact", record["command"])
        for index in range(2):
            names = {r["environment"]["PBPS_TEST_PG_NAME"] for r in calls
                     if r["environment"].get("PBPS_LIVE_SHARD") == f"{index}/2"}
            self.assertEqual(names, {f"pbps-test-shards{index}-pg"})
        transport = [r for r in calls if r["command"][:2] == ["python3", "scripts/live-transport.py"]]
        self.assertEqual(len(transport), 1)

    def test_missing_duplicate_failed_or_empty_libtest_reports_fail_the_driver(self):
        for mode in ("missing", "duplicate", "failed", "empty"):
            with self.subTest(mode=mode):
                result, _, summary, _ = self.fake_run("pg", 1, mode)
                self.assertNotEqual(result.returncode, 0)
                self.assertTrue(summary)
                self.assertTrue(summary["pg"]["failed"] or summary["pg"]["union"] == "failed")

    def test_direct_stderr_diagnostics_cannot_split_a_named_libtest_result(self):
        result, _, summary, _ = self.fake_run("pg", 1, "stderr")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(summary["pg"]["union"], "exact")
        self.assertEqual(summary["pg"]["failed"], 0)

    def test_an_accepted_zero_index_keeps_the_shard_zero_transport_obligation(self):
        result, calls, _, _ = self.fake_run("pg", None, selector="00/1")
        self.assertEqual(result.returncode, 0, result.stderr)
        transport = [r for r in calls if r["command"][:2] == ["python3", "scripts/live-transport.py"]]
        self.assertEqual(len(transport), 1)
        self.assertEqual(transport[0]["environment"]["PBPS_LIVE_SHARD"], "0/1")

    def test_whole_suite_empty_shard_fails_even_when_each_union_is_exact(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            for i, assigned in enumerate((["one"], [])):
                (root / str(i)).mkdir()
                row = {"args": [], "identity": "bin", "listed": ["one"],
                       "groups": [["one"], []], "assigned": assigned, "pinned": False,
                       "reported": {n: {"status": "ok", "seconds": 1} for n in assigned},
                       "valid": True}
                (root / str(i) / "call.json").write_text(json.dumps(row))
            with self.assertRaisesRegex(ValueError, "empty for the whole suite"):
                SHARDS.audit(root, 2)

    def test_receipt_audit_independently_rejects_missing_duplicate_and_unreported_tests(self):
        for second in ("b", "c", "a", None):
            with tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                groups = [["a"], [second or "b"]]
                for index, assigned in enumerate(groups):
                    (root / str(index)).mkdir()
                    row = {"args": [], "identity": "bin", "listed": ["a", "b"],
                           "groups": groups, "assigned": assigned, "pinned": False,
                           "reported": {name: {"status": "ok", "seconds": 1} for name in assigned},
                           "valid": True}
                    if second is None and index == 1:
                        row["reported"] = {}
                    (root / str(index) / "call.json").write_text(json.dumps(row))
                if second == "b":
                    summary, _ = SHARDS.audit(root, 2)
                    self.assertEqual(summary, {"passed": 2, "failed": 0, "union": "exact"})
                else:
                    with self.assertRaises(ValueError):
                        SHARDS.audit(root, 2)

    def test_unreadable_lists_and_invalid_weights_are_not_empty_successes(self):
        for listing in ("", "one: test\n", "one: test\n2 tests, 0 benchmarks\n",
                        "one: test\none: test\n2 tests, 0 benchmarks\n"):
            with self.assertRaises(ValueError):
                SHARDS.listed_tests(listing)
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "weights.json"
            self.assertIsNone(SHARDS.read_weights(path))
            path.write_text("{}")
            self.assertEqual(SHARDS.read_weights(path), {})
            for content in ("{", "[]", '{"bin:one": -1}', '{"bin:one": "slow"}', '{"x": NaN}'):
                path.write_text(content)
                with self.assertRaises(ValueError):
                    SHARDS.read_weights(path)

    def fake_run(self, suite, count, mode="ok", selector=None, existing=(), remaining=None,
                 interrupt=None, repeat_prefix=None):
        with tempfile.TemporaryDirectory(prefix="pbps-shard-regression-") as directory:
            root = Path(directory)
            containers = root / "containers"
            containers.mkdir()
            for name in existing:
                (containers / name).touch()
            clients = root / "clients"
            clients.mkdir()
            fake = clients / "client"
            fake.write_text(f"#!{sys.executable}\n" + r'''
import json, os, shutil, signal, subprocess, sys, time
from pathlib import Path
root = Path(os.environ["PBPS_SHARD_FAKE_ROOT"])
command = [Path(sys.argv[0]).name, *sys.argv[1:]]
if command[0] == "python3" and Path(command[1]).name == "live-shards.py":
    sys.exit(subprocess.run([sys.executable, *command[1:]]).returncode)
record = {"command": command, "environment": {k: v for k, v in os.environ.items()
          if k.startswith(("PBPS_TEST_", "PBPS_LIVE_"))}}
with (root / "calls").open("a") as log:
    log.write(json.dumps(record) + "\n")
if command[:2] == ["docker", "ps"]:
    if os.environ["PBPS_SHARD_FAKE_MODE"] == "list-failed" or (
            os.environ["PBPS_SHARD_FAKE_MODE"] == "cleanup-list-failed" and (root / "tested").exists()):
        print("listing denied", file=sys.stderr)
        sys.exit(1)
    for path in (root / "containers").iterdir():
        print(path.name)
elif command[:2] == ["docker", "rm"]:
    assert command[2:4] == ["-f", "-v"] and len(command) == 5, command
    if os.environ["PBPS_SHARD_FAKE_MODE"] == "cleanup-failed" and command[-1].endswith("s0-pg"):
        print("removal denied", file=sys.stderr)
        sys.exit(1)
    (root / "containers" / command[-1]).unlink()
elif command[0] == "free":
    print("Mem: 65536 0 65536 0 0 65536")
elif command[:2] == ["docker", "exec"]:
    if "SELECT CONVERT" in command[-1]:
        print("SQL_Latin1_General_CP1_CI_AS")
elif command[:2] == ["docker", "run"]:
    binding = command[command.index("-p") + 1]
    occupied = any(p.read_text() == binding for p in (root / "containers").iterdir())
    (root / "containers" / command[command.index("--name") + 1]).write_text(binding)
    if occupied or os.environ["PBPS_SHARD_FAKE_MODE"] == "bind-failed":
        print("failed to bind host port", file=sys.stderr)
        sys.exit(1)
elif command[0] == "sleep":
    pass
elif command[:2] == ["python3", "scripts/live-transport.py"]:
    for name in ("tls_a", "tls_b"):
        subprocess.run(["cargo", "test", "-p", "pbps-db", "--test", "live_transport", "--",
                        "--ignored", "--exact", name, "--test-threads=1", "--format=pretty",
                        "--color=never"], check=True)
elif command[0] == "cargo":
    tail = iter(command[command.index("--") + 1:])
    filters = []
    for arg in tail:
        if arg in ("--skip", "--test-threads", "--format", "--color"):
            next(tail)
        elif not arg.startswith("-"):
            filters.append(arg)
    if "--exact" in command:
        names = filters
    elif "type_slots" in command:
        names = ["one_case"]
    else:
        names = [f"{filters[0] if filters else 'fixture'}::case{i:02}" for i in range(12)]
        if not filters and os.environ["PBPS_SHARD_FAKE_SUITE"] == "mssql" and os.environ["PBPS_SHARD_FAKE_MODE"] != "express-missing":
            inventory = json.loads(os.environ["PBPS_SHARD_FAKE_EXPRESS"])
            names += inventory.get(command[command.index("-p") + 1], [])
    if "--list" in command:
        print("\n".join(n + ": test" for n in names))
        print(f"\n{len(names)} tests, 0 benchmarks")
    else:
        mode = os.environ["PBPS_SHARD_FAKE_MODE"]
        (root / "tested").touch()
        if mode == "interrupt":
            signal.signal(signal.SIGINT, signal.SIG_IGN)
            signal.signal(signal.SIGTERM, signal.SIG_IGN)
            (root / ("waiting-" + str(os.getpid()))).touch()
            while True:
                time.sleep(1)
        selected = names[1:] if mode == "missing" else ([] if mode == "empty" else names)
        print(f"\nrunning {len(selected)} tests")
        for index, name in enumerate(selected):
            if mode == "stderr":
                print(f"test {name} ... ", end="", flush=True)
                print("controlled direct stderr diagnostic", file=sys.stderr, flush=True)
                print("ok", flush=True)
            else:
                print(f"test {name} ... {'FAILED' if mode == 'failed' and index == 0 else 'ok'}")
            if mode == "duplicate" and index == 0:
                print(f"test {name} ... ok")
        failed = int(mode == "failed" and bool(selected))
        print(f"\ntest result: {'FAILED' if failed else 'ok'}. {len(selected)-failed} passed; "
              f"{failed} failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s")
        sys.exit(101 if failed else 0)
elif command[:2] != ["python3", "scripts/check-live-fixture.py"]:
    raise RuntimeError("unexpected external client: " + repr(command))
''')
            fake.chmod(0o755)
            for name in ("cargo", "docker", "python3", "sleep", "free"):
                (clients / name).symlink_to(fake)
            output = root / "results"
            env = fixture_environment(PATH=str(clients) + os.pathsep + os.environ["PATH"],
                                      PBPS_SHARD_FAKE_ROOT=str(root), PBPS_SHARD_FAKE_MODE=mode,
                                      PBPS_SHARD_FAKE_SUITE=suite,
                                      PBPS_SHARD_FAKE_EXPRESS=json.dumps({cargo[2]: sorted(names)
                                          for cargo, names in SHARDS.EXPRESS_INVENTORY.items()}))
            if selector is not None:
                env.update(PBPS_LIVE_SHARD=selector,
                           PBPS_LIVE_SHARD_DIR=str(output / suite / "0"))
            if count is None:
                command = ["bash", "scripts/" + SHARDS.RUNNERS[suite]]
            else:
                command = [sys.executable, "scripts/live-shards.py", "--suite", suite,
                           "--" + suite + "-shards", str(count), "--prefix", "pbps-test-shard",
                           "--output", str(output), "--measure", str(root / "weights.json")]
            if interrupt is not None:
                # Exercise escalation without waiting the production grace.
                entry = root / "driver.py"
                entry.write_text("import importlib.util, sys\n"
                                 f"spec = importlib.util.spec_from_file_location('driver', {str(SHARDS.SCRIPT)!r})\n"
                                 "driver = importlib.util.module_from_spec(spec)\n"
                                 "spec.loader.exec_module(driver)\n"
                                 "driver.RUNNER_STOP_GRACE_SECONDS = 0.2\n"
                                 "sys.exit(driver.main())\n")
                command[1] = str(entry)
            with subprocess.Popen(command, cwd=ROOT, env=env, text=True,
                                  stdout=subprocess.PIPE, stderr=subprocess.PIPE) as process:
                if interrupt is not None:
                    deadline = time.monotonic() + 20
                    while not list(root.glob("waiting-*")) and process.poll() is None and time.monotonic() < deadline:
                        time.sleep(0.01)
                    self.assertTrue(list(root.glob("waiting-*")), "runner must reach the interrupt barrier")
                    process.send_signal(interrupt)
                try:
                    stdout, stderr = process.communicate(timeout=60 if interrupt is None else 10)
                except subprocess.TimeoutExpired:
                    # Keep a broken implementation from stranding fake clients.
                    for path in root.glob("waiting-*"):
                        try:
                            os.kill(int(path.name.split("-")[1]), signal.SIGKILL)
                        except ProcessLookupError:
                            pass
                    process.kill()
                    process.communicate(timeout=10)
                    self.fail("driver did not stop runner descendants and clean up")
                result = subprocess.CompletedProcess(command, process.returncode, stdout, stderr)
            if repeat_prefix is not None:
                self.assertEqual(result.returncode, 0, result.stderr)
                command[command.index("--prefix") + 1] = repeat_prefix
                output = root / "next-results"
                command[command.index("--output") + 1] = str(output)
                result = subprocess.run(command, cwd=ROOT, env=env, text=True,
                                        capture_output=True, timeout=60)
            if remaining is not None:
                remaining.extend(p.name for p in containers.iterdir())
            for path in root.glob("waiting-*"):
                status = Path("/proc") / path.name.split("-")[1] / "status"
                if status.exists():
                    state = status.read_text()
                    if not re.search(r"State:\s+Z", state):
                        try:
                            os.kill(int(path.name.split("-")[1]), signal.SIGKILL)
                        except ProcessLookupError:
                            pass
                    self.assertRegex(state, r"State:\s+Z", "interrupted client must be stopped")
            calls = [json.loads(line) for line in (root / "calls").read_text().splitlines()]
            summary = json.loads((output / "summary.json").read_text()) if (output / "summary.json").exists() else {}
            receipts = [json.loads(p.read_text()) for p in output.glob("*/*/*.json")]
            return result, calls, summary, receipts


class LiveExecution(unittest.TestCase):
    def test_fixture_names_default_when_unset_or_empty(self):
        for runner in ("mssql", "pg", "resolver"):
            for empty in (False, True):
                with self.subTest(runner=runner, empty=empty):
                    overrides = dict.fromkeys(FIXTURE_NAMES, "") if empty else {}
                    result, calls, _ = self.named_fixture_script(runner, overrides)
                    self.assertEqual(result.returncode, 0, result.stderr)
                    self.assertEqual(self.started_names(calls), self.fixture_names(runner))

    def test_custom_names_reach_every_fixture_consumer_without_changing_test_commands(self):
        overrides = {key: "lane." + value + ".fixture" for key, value in FIXTURE_NAMES.items()}
        ports = {"PBPS_TEST_PORT": "25330", "PBPS_TEST_EXPRESS_PORT": "25331",
                 "PBPS_TEST_MSSQL_SCRATCH_PORT": "25332", "PBPS_TEST_PG_PORT": "55320",
                 "PBPS_TEST_PG_OLD_PORT": "55321", "PBPS_TEST_PG_SCRATCH_PORT": "55322"}
        for runner in ("mssql", "pg", "resolver"):
            with self.subTest(runner=runner):
                result, calls, environments = self.named_fixture_script(runner, overrides | ports)
                self.assertEqual(result.returncode, 0, result.stderr)
                expected = self.fixture_names(runner, "lane.", ".fixture")
                self.assertEqual(self.started_names(calls), expected)
                self.assertEqual({c[2] for c in calls if c[:2] == ["docker", "exec"]},
                                 set(expected))
                validators = [c for c in calls if c[:2] == ["python3", "scripts/check-live-fixture.py"]]
                checked = [c[c.index("--container") + 1] for c in validators]
                self.assertEqual(checked, expected if runner != "mssql" else
                                 [expected[0], expected[1], expected[0], expected[2]])
                for call in validators:
                    name = call[call.index("--container") + 1]
                    key = next(key for key, value in overrides.items() if value == name)
                    port_key = {"PBPS_TEST_MSSQL_NAME": "PBPS_TEST_PORT",
                                "PBPS_TEST_MSSQL_EXPRESS_NAME": "PBPS_TEST_EXPRESS_PORT"}.get(
                                    key, key.removesuffix("_NAME") + "_PORT")
                    self.assertEqual(call[call.index("--port") + 1], ports[port_key])
                if runner == "pg":
                    installs = [c[2] for c in calls if c[:2] == ["docker", "exec"]
                                and "partman" in " ".join(c)]
                    self.assertCountEqual(installs, expected[:2])
                    self.assertTrue(environments)
                    for env in environments:
                        self.assertEqual(env["PBPS_TEST_PG_CONTAINER"], expected[0])
                        self.assertEqual(env["PBPS_TEST_PG_OLD_CONTAINER"], expected[1])
                baseline, default_calls, _ = self.named_fixture_script(runner)
                self.assertEqual(baseline.returncode, 0, baseline.stderr)
                clients = lambda commands: [c for c in commands if c[0] == "cargo"
                                           or c[:2] == ["python3", "scripts/live-transport.py"]]
                self.assertEqual(clients(calls), clients(default_calls))
                self.assertTrue(all(c[-2:] == ["--exact", "fixture case"]
                                    for c in calls if c[0] == "cargo"))

    def test_dotted_names_reuse_only_exact_existing_containers(self):
        overrides = {key: "lane." + value + ".fixture" for key, value in FIXTURE_NAMES.items()}
        for runner in ("mssql", "pg", "resolver"):
            expected = self.fixture_names(runner, "lane.", ".fixture")
            for exact in (False, True):
                with self.subTest(runner=runner, exact=exact):
                    # The regex-like neighbour is a valid container name too.
                    existing = expected if exact else [name.replace(".", "X") for name in expected]
                    result, calls, _ = self.named_fixture_script(runner, overrides, existing)
                    self.assertEqual(result.returncode, 0, result.stderr)
                    self.assertEqual(self.started_names(calls), [] if exact else expected)
                    self.assertFalse(any(c[:2] == ["docker", "rm"] for c in calls))

    def test_mssql_scratch_defaults_to_the_resolved_primary_name(self):
        for runner in ("mssql", "resolver"):
            with self.subTest(runner=runner):
                result, calls, _ = self.named_fixture_script(
                    runner, {"PBPS_TEST_MSSQL_NAME": "lane.primary"})
                self.assertEqual(result.returncode, 0, result.stderr)
                expected = ["lane.primary", "lane.primary-scratch"]
                if runner == "mssql":
                    expected.insert(1, "pbps-test-mssql-express")
                self.assertEqual(self.started_names(calls), expected)

    def test_resolver_inherited_primary_precedes_the_name_override(self):
        result, calls, _ = self.named_fixture_script(
            "resolver", {"NAME": "caller.primary", "PBPS_TEST_MSSQL_NAME": "other.primary",
                         "PBPS_TEST_MSSQL_SCRATCH_NAME": "explicit.scratch"})
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.started_names(calls), ["caller.primary", "explicit.scratch"])

    def test_custom_fixture_failures_log_the_selected_name_and_preserve_it(self):
        cases = [("mssql", "PBPS_TEST_MSSQL_EXPRESS_NAME"),
                 ("pg", "PBPS_TEST_PG_NAME"), ("pg", "PBPS_TEST_PG_OLD_NAME"),
                 ("pg", "PBPS_TEST_PG_SCRATCH_NAME"),
                 ("resolver", "PBPS_TEST_MSSQL_NAME"),
                 ("resolver", "PBPS_TEST_MSSQL_SCRATCH_NAME")]
        for runner, key in cases:
            with self.subTest(runner=runner, key=key):
                result, calls, _ = self.named_fixture_script(
                    runner, {key: "lane.failed"}, failing="lane.failed")
                self.assertNotEqual(result.returncode, 0)
                self.assertIn(["docker", "logs", "lane.failed"], calls)
                self.assertFalse(any(c[0] == "cargo" or c[:2] == ["docker", "rm"] for c in calls))

    @staticmethod
    def fixture_names(runner, prefix="", suffix=""):
        keys = list(FIXTURE_NAMES)
        selected = keys[3:] if runner == "pg" else keys[:3]
        if runner == "resolver":
            selected = [keys[0], keys[2]]
        return [prefix + FIXTURE_NAMES[key] + suffix for key in selected]

    @staticmethod
    def started_names(calls):
        return [c[c.index("--name") + 1] for c in calls if c[:2] == ["docker", "run"]]

    def named_fixture_script(self, runner, overrides=None, existing=(), failing=""):
        # Exercise the real scripts, including the sourced helper. Every Docker,
        # checker, transport and cargo invocation is intercepted before execution.
        with tempfile.TemporaryDirectory(prefix="pbps-fixture-names-") as directory:
            root = Path(directory)
            fake = root / "client"
            fake.write_text(f"#!{sys.executable}\n" + r'''
import json, os, sys
from pathlib import Path
root = Path(os.environ["PBPS_NAMES_CONTROL"])
command = [Path(sys.argv[0]).name, *sys.argv[1:]]
record = {"command": command}
if command[0] == "cargo":
    record["environment"] = {key: os.environ.get(key) for key in
                             ("PBPS_TEST_PG_CONTAINER", "PBPS_TEST_PG_OLD_CONTAINER")}
with (root / "calls").open("a") as log:
    log.write(json.dumps(record) + "\n")
if command[:2] == ["docker", "ps"]:
    print(os.environ["PBPS_NAMES_EXISTING"])
    if (root / "started").exists():
        print((root / "started").read_text())
elif command[:2] == ["docker", "run"]:
    with (root / "started").open("a") as started:
        started.write(command[command.index("--name") + 1] + "\n")
elif command[:2] == ["docker", "exec"]:
    if command[2] == os.environ["PBPS_NAMES_FAILING"]:
        sys.exit(1)
    if "SELECT CONVERT" in command[-1]:
        print("SQL_Latin1_General_CP1_CI_AS")
elif command[:2] == ["docker", "logs"]:
    print("controlled fixture not ready", file=sys.stderr)
elif command[0] not in ("cargo", "python3", "sleep"):
    raise RuntimeError("Unexpected external command: " + repr(command))
''')
            fake.chmod(0o755)
            for client in ("docker", "cargo", "python3", "sleep"):
                (root / client).symlink_to(fake)
            env = fixture_environment(
                PATH=str(root) + os.pathsep + os.environ["PATH"],
                PBPS_NAMES_CONTROL=str(root), PBPS_NAMES_EXISTING="\n".join(existing),
                PBPS_NAMES_FAILING=failing)
            env.update(overrides or {})
            command = (["bash", "-c", "source scripts/resolver-mssql-fixtures.sh"]
                       if runner == "resolver" else
                       ["bash", "scripts/live-tests" + ("-pg" if runner == "pg" else "") + ".sh",
                        "--exact", "fixture case"])
            result = subprocess.run(command, cwd=ROOT, env=env, text=True,
                                    capture_output=True, timeout=30)
            records = [json.loads(line) for line in (root / "calls").read_text().splitlines()]
            return result, [r["command"] for r in records], [r["environment"] for r in records
                                                            if "environment" in r]

    def test_resolver_fixtures_inherit_the_primary_name_and_reuse_exact_matches(self):
        for primary in (None, "pbps2236.fixture"):
            for reuse in (False, True):
                with self.subTest(primary=primary, reuse=reuse):
                    name = primary or "pbps-test-mssql"
                    scratch = name + "-scratch"
                    existing = name if reuse else (
                        name.replace(".", "X") if primary else name + "-unrelated")
                    with tempfile.TemporaryDirectory(prefix="pbps-mssql-names-") as directory:
                        root = Path(directory)
                        fake = root / "client"
                        fake.write_text(f"#!{sys.executable}\n" + r'''
import json, os, sys
from pathlib import Path
root = Path(os.environ["PBPS_FIXTURE_CONTROL"])
command = [Path(sys.argv[0]).name, *sys.argv[1:]]
with (root / "calls").open("a") as log:
    log.write(json.dumps(command) + "\n")
if command[:2] == ["docker", "ps"]:
    print(os.environ["PBPS_FIXTURE_EXISTING"])
elif command[:2] == ["docker", "exec"]:
    if "SELECT CONVERT" in command[-1]:
        print("SQL_Latin1_General_CP1_CI_AS")
elif command[:2] != ["docker", "run"] and command[0] != "python3":
    raise RuntimeError("Unexpected external command: " + repr(command))
''')
                        fake.chmod(0o755)
                        for client in ("docker", "python3"):
                            (root / client).symlink_to(fake)
                        env = fixture_environment(
                                   PATH=str(root) + os.pathsep + os.environ["PATH"],
                                   PBPS_FIXTURE_CONTROL=str(root),
                                   PBPS_FIXTURE_EXISTING=existing)
                        env.pop("NAME", None)
                        if primary:
                            env["NAME"] = primary
                        # No fixtures or live suites run: only the sourced helper's
                        # real control flow reaches the fake external clients.
                        result = subprocess.run(
                            ["bash", "-c", "source scripts/resolver-mssql-fixtures.sh"],
                            cwd=ROOT, env=env, text=True, capture_output=True, timeout=10)
                        self.assertEqual(result.returncode, 0, result.stderr)
                        calls = [json.loads(line) for line in
                                 (root / "calls").read_text().splitlines()]
                        starts = [c[c.index("--name") + 1] for c in calls
                                  if c[:2] == ["docker", "run"]]
                        expected = [scratch] if reuse else [name, scratch]
                        self.assertEqual(starts, expected)
                        self.assertEqual({c[2] for c in calls if c[:2] == ["docker", "exec"]},
                                         {name, scratch})
                        validators = [c[c.index("--container") + 1] for c in calls
                                      if c[0] == "python3"]
                        self.assertEqual(validators, [name, scratch])

    def test_custom_collation_must_finish_before_clients_start(self):
        for runner in ('compatibility', 'local', 'ci'):
            with self.subTest(runner=runner):
                result, calls = self.collation_script(runner, 'initializing')
                self.assertEqual(result.returncode, 0, result.stderr)
                probes = [c for c in calls if c[:2] == ['docker', 'exec']
                          and 'express' in c[2]]
                self.assertEqual(len(probes), 3)
                self.assertTrue(any(c[0] == 'cargo' for c in calls))

    def test_missing_custom_collation_stops_before_clients_and_preserves_fixture(self):
        for runner, limit in (('compatibility', 90), ('local', 60), ('ci', 60)):
            with self.subTest(runner=runner):
                result, calls = self.collation_script(runner, 'never-ready')
                self.assertNotEqual(result.returncode, 0)
                probes = [c for c in calls if c[:2] == ['docker', 'exec']
                          and 'express' in c[2]]
                self.assertEqual(len(probes), limit)
                self.assertFalse(any(c[0] == 'cargo' for c in calls))
                self.assertTrue(any(c[:2] == ['docker', 'logs'] for c in calls))
                self.assertFalse(any(c[:2] == ['docker', 'rm'] for c in calls))

    def collation_script(self, runner, mode):
        # Execute each real shell path. The native measurement records a
        # successful login before the requested collation becomes visible.
        with tempfile.TemporaryDirectory(prefix='pbps-collation-admission-') as directory:
            root = Path(directory)
            fake = root / 'client'
            fake.write_text(f'#!{sys.executable}\n' + r'''
import json, os, subprocess, sys
from pathlib import Path
root = Path(os.environ['PBPS_COLLATION_CONTROL'])
command = [Path(sys.argv[0]).name, *sys.argv[1:]]
with (root / 'calls').open('a') as log:
    log.write(json.dumps(command) + '\n')
if command[:2] == ['docker', 'ps']:
    pass
elif command[:2] == ['docker', 'run']:
    print('owned-fixture')
elif command[:2] == ['docker', 'exec']:
    if 'express' not in command[2]:
        if "SERVERPROPERTY('Collation')" in command[command.index('-Q') + 1]:
            print('SQL_Latin1_General_CP1_CI_AS')
        sys.exit(0)
    state = root / 'probes'
    count = int(state.read_text()) + 1 if state.exists() else 1
    state.write_text(str(count))
    query = command[command.index('-Q') + 1]
    guarded = ('-b' in command and "SERVERPROPERTY('Collation')" in query
               and 'Latin1_General_100_CS_AS' in query and 'THROW' in query)
    # The old SELECT 1 succeeds immediately, despite the wrong identity.
    sys.exit(1 if guarded and (count < 3 or os.environ['PBPS_COLLATION_MODE'] == 'never-ready') else 0)
elif command[:2] == ['docker', 'logs']:
    print('controlled collation change pending', file=sys.stderr)
elif command[:2] == ['python3', '-c']:
    sys.exit(subprocess.run([sys.executable, *command[1:]]).returncode)
elif command[0] in ('cargo', 'python3'):
    if os.environ['PBPS_COLLATION_MODE'] == 'never-ready' or int((root / 'probes').read_text()) < 3:
        sys.exit('client reached the pre-collation server')
elif command[0] != 'sleep':
    raise RuntimeError('Unexpected external command: ' + repr(command))
''')
            fake.chmod(0o755)
            for name in ('docker', 'cargo', 'python3', 'sleep'):
                (root / name).symlink_to(fake)
            env = fixture_environment(PATH=str(root) + os.pathsep + os.environ['PATH'],
                       PBPS_COLLATION_CONTROL=str(root), PBPS_COLLATION_MODE=mode,
                       IMAGE='owned-image')
            if runner == 'ci':
                workflow = (ROOT / '.github/workflows/ci.yml').read_text()
                block = workflow.split('      - name: start the two SQL Servers, one after the other\n', 1)[1]
                block = block.split('        run: |\n', 1)[1].split('      # A name filter', 1)[0]
                command = ['bash', '-e', '-c', textwrap.dedent(block) + '\ncargo test\n']
            elif runner == 'local':
                command = ['bash', 'scripts/live-tests.sh']
            else:
                command = ['bash', 'scripts/compatibility-tests.sh', 'mssql2025-express']
            result = subprocess.run(command, cwd=ROOT, env=env, capture_output=True,
                                    text=True, timeout=30)
            calls = [json.loads(line) for line in (root / 'calls').read_text().splitlines()]
            return result, calls

    def test_driver_error_regressions_run_in_their_engine_jobs(self):
        workflow = (ROOT / ".github/workflows/ci.yml").read_text()
        for job, target in (("live", "live_mssql"), ("live-pg", "live_pg")):
            with self.subTest(job=job):
                command = ["cargo", "test", "-p", "pbps-db", "--test", target,
                           "--", "--ignored"]
                self.assertIn(command, job_commands(workflow, job))

    def test_a_comment_or_another_job_does_not_run_the_missing_target(self):
        command = "cargo test -p pbps-db --test live_pg -- --ignored"
        workflow = ("jobs:\n  live-pg:\n      # - run: " + command +
                    "\n  live:\n      - run: " + command + "\n")
        self.assertNotIn(shlex.split(command), job_commands(workflow, "live-pg"))
        self.assertEqual(job_commands(workflow, "absent"), [])

    def test_ci_postgresql_health_checks_refuse_the_initialization_server(self):
        workflow = (ROOT / ".github/workflows/ci.yml").read_text()
        commands = [shlex.split(value) for value in
                    re.findall(r'--health-cmd "(pg_isready[^"\n]+)"', workflow)]
        # The target, the pre-17 server and the resolver's scratch cluster.
        self.assertEqual(len(commands), 3)
        for command in commands:
            with self.subTest(command=command):
                # Socket readiness is true while docker-entrypoint.sh is
                # initializing; the measured temporary server has no TCP.
                accepts_temporary = "-h" not in command or command[command.index("-h") + 1].startswith("/")
                self.assertFalse(accepts_temporary)
                self.assertEqual(command[command.index("-h") + 1], "127.0.0.1")

    def test_local_postgresql_deadlines_stop_before_any_client_suite(self):
        for mode, failing in (("deadline-new", "pbps-test-pg"),
                              ("deadline-old", "pbps-test-pg16"),
                              ("deadline-scratch", "pbps-test-pg-scratch")):
            with self.subTest(mode=mode):
                result, calls = self.local_pg_script(mode)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn(failing + " did not become TCP-ready", result.stderr)
                self.assertFalse(any(c[0] in ("cargo", "python3") for c in calls))
                probes = [c for c in calls if c[:3] == ["docker", "exec", failing]]
                self.assertEqual(len(probes), 60)
                self.assertIn(["docker", "logs", failing], calls)
                self.assertFalse(any(c[:2] == ["docker", "rm"] for c in calls))

    def test_local_postgresql_clients_wait_for_both_final_tcp_servers(self):
        result, calls = self.local_pg_script("initializing")
        self.assertEqual(result.returncode, 0, result.stderr)
        first_client = next(i for i, c in enumerate(calls) if c[0] == "cargo")
        for name in ("pbps-test-pg", "pbps-test-pg16", "pbps-test-pg-scratch"):
            probes = [c for c in calls[:first_client]
                      if c[:4] == ["docker", "exec", name, "pg_isready"]]
            self.assertEqual(len(probes), 3)
        # pg_partman (#1533) goes onto the two servers the flow cases use,
        # once each is ready and before any client runs; never the scratch.
        last_probe = max(i for i, c in enumerate(calls) if c[3:4] == ["pg_isready"])
        installs = [(i, c[2]) for i, c in enumerate(calls)
                    if c[:2] == ["docker", "exec"] and "partman" in " ".join(c)]
        # In either order: the two run at once.
        self.assertCountEqual([name for _, name in installs], ["pbps-test-pg", "pbps-test-pg16"])
        self.assertTrue(all(last_probe < i < first_client for i, _ in installs), installs)
        self.assertEqual([c for c in calls if c[0] in ("cargo", "python3")], [
            ["python3", "scripts/check-live-fixture.py", "--cell", "pg18", "--container", "pbps-test-pg", "--port", "54320"],
            ["python3", "scripts/check-live-fixture.py", "--cell", "pg16", "--container", "pbps-test-pg16", "--port", "54321"],
            ["python3", "scripts/check-live-fixture.py", "--cell", "pg18", "--container", "pbps-test-pg-scratch", "--port", "54322"],
            ["cargo", "test", "-p", "pbps-pg", "--test", "live", "--", "--ignored"],
            ["cargo", "test", "-p", "pbps-pg", "--test", "type_slots", "--", "--ignored"],
            ["cargo", "test", "--profile", "live-test", "-p", "pbps-pg", "--lib", "--", "--ignored", "--test-threads=1"],
            ["cargo", "test", "-p", "pbps-db", "--test", "live_pg", "--", "--ignored"],
            ["python3", "scripts/live-transport.py", "pg"],
            ["cargo", "test", "-p", "pbps-cli", "--bin", "pbps", "--", "--ignored", "--test-threads=1"],
            ["cargo", "test", "--profile", "live-test", "-p", "pbps-cli", "--lib", "--", "--ignored", "--test-threads=1", "resolver::server::scratch::live_tests"],
            ["cargo", "test", "-p", "pbps-cli", "--test", "flow_pg", "--", "--ignored", "--test-threads=1"],
        ])

    def test_wrong_fixture_identity_stops_before_test_writes(self):
        result, calls = self.local_pg_script("wrong-identity")
        self.assertNotEqual(result.returncode, 0)
        self.assertFalse(any(c[0] == "cargo" for c in calls))
        self.assertFalse(any(c[:2] == ["docker", "rm"] for c in calls))

    def local_pg_script(self, mode):
        # Run the real shell control flow. Only external clients are replaced;
        # no database is started and the production loop limit is retained.
        with tempfile.TemporaryDirectory(prefix="pbps-pg-admission-") as directory:
            root = Path(directory)
            fake = root / "client"
            fake.write_text(f"#!{sys.executable}\n" + r'''
import json, os, sys
from pathlib import Path
root = Path(os.environ["PBPS_ADMISSION_CONTROL"])
command = [Path(sys.argv[0]).name, *sys.argv[1:]]
with (root / "calls").open("a") as log:
    log.write(json.dumps(command) + "\n")
if command[:2] == ["docker", "ps"]:
    print("pbps-test-pg\npbps-test-pg16\npbps-test-pg-scratch")
elif command[:2] == ["docker", "exec"]:
    name = command[2]
    state = root / name
    count = int(state.read_text()) + 1 if state.exists() else 1
    state.write_text(str(count))
    mode = os.environ["PBPS_ADMISSION_MODE"]
    tcp = "-h" in command and command[command.index("-h") + 1] == "127.0.0.1"
    deadline = (mode == "deadline-new" and name == "pbps-test-pg" or
                mode == "deadline-old" and name == "pbps-test-pg16" or
                mode == "deadline-scratch" and name == "pbps-test-pg-scratch")
    sys.exit(2 if tcp and (deadline or mode == "initializing" and count <= 2) else 0)
elif command[:2] == ["docker", "logs"]:
    print("controlled server still initializing", file=sys.stderr)
elif command[:2] == ["python3", "scripts/check-live-fixture.py"]:
    sys.exit(1 if os.environ["PBPS_ADMISSION_MODE"] == "wrong-identity" else 0)
elif command[0] not in ("cargo", "python3", "sleep"):
    raise RuntimeError("Unexpected external command: " + repr(command))
''')
            fake.chmod(0o755)
            for name in ("docker", "cargo", "python3", "sleep"):
                (root / name).symlink_to(fake)
            env = fixture_environment(PATH=str(root) + os.pathsep + os.environ["PATH"],
                       PBPS_ADMISSION_CONTROL=str(root), PBPS_ADMISSION_MODE=mode)
            result = subprocess.run(["bash", "scripts/live-tests-pg.sh"], cwd=ROOT,
                                    env=env, text=True, capture_output=True, timeout=30)
            calls = [json.loads(line) for line in (root / "calls").read_text().splitlines()]
            return result, calls

if __name__ == "__main__":
    unittest.main()
