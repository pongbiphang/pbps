#!/usr/bin/env python3
"""Regressions for live-test execution ownership, without starting fixtures."""

import contextlib
import importlib.util
import io
from pathlib import Path
import re
import shlex
import subprocess
import unittest
from unittest.mock import patch


ROOT = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location(
    "native_target", ROOT / "scripts/live-resolver-target.py"
)
native = importlib.util.module_from_spec(spec)
spec.loader.exec_module(native)


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


class LiveExecution(unittest.TestCase):
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

    def test_native_root_runner_executes_daemon_target_and_factory(self):
        env = {"PBPS_RESOLVER_TEST_SOCKET": "/owned/docker.sock",
               "PBPS_RESOLVER_TEST_IMAGE": "owned-image",
               "PBPS_NATIVE_DRIVER": "pg"}
        completed = subprocess.CompletedProcess([], 0, "test result: ok. 1 passed\n")
        with patch.object(native, "run", return_value=completed) as run:
            with contextlib.redirect_stdout(io.StringIO()):
                native.native_tests("/owned/tests", env)
        self.assertEqual([call.args for call in run.call_args_list], [
            ("/owned/tests", "--ignored", "--exact", name, "--nocapture")
            for name in (
                "resolver::docker::tests::direct_native_daemon_is_accepted_but_a_root_owned_proxy_is_not",
                native.TARGET_TEST, native.FACTORY_TEST, native.RECIPE_TEST,
                native.ANALYSIS_TEST, native.INVALIDATION_TEST, native.CANCELLATION_TEST,
            )
        ])
        for call in run.call_args_list:
            for key, value in env.items():
                self.assertEqual(call.kwargs["env"][key], value)

    def test_sql_server_native_runner_keeps_its_original_three_cases(self):
        completed = subprocess.CompletedProcess([], 0, "test result: ok. 1 passed\n")
        with patch.object(native, "run", return_value=completed) as run:
            with contextlib.redirect_stdout(io.StringIO()):
                native.native_tests("/owned/tests", {"PBPS_NATIVE_DRIVER": "mssql"})
        self.assertEqual([call.args[3] for call in run.call_args_list],
                         [native.DAEMON_TEST, native.TARGET_TEST, native.FACTORY_TEST])

    def test_pinned_pg16_selector_keeps_the_default_and_sql_server_images(self):
        self.assertEqual(native.fixture_image("pg", 16), native.PG_IMAGES[16])
        self.assertEqual(native.fixture_image("pg", 18), native.IMAGES["pg"])
        self.assertNotEqual(native.PG_IMAGES[16], native.PG_IMAGES[18])
        self.assertEqual(native.fixture_image("mssql", 18), native.IMAGES["mssql"])
        for image in native.PG_IMAGES.values():
            self.assertRegex(image, r"^postgres@sha256:[0-9a-f]{64}$")

    def test_a_missing_or_failing_native_case_stops_the_fixture(self):
        for code, output in ((0, "test result: ok. 0 passed"),
                             (1, "test result: FAILED. 0 passed"),
                             (1, "test result: ok. 1 passed")):
            with self.subTest(code=code, output=output):
                completed = subprocess.CompletedProcess([], code, output)
                with patch.object(native, "run", return_value=completed) as run:
                    with contextlib.redirect_stdout(io.StringIO()):
                        with self.assertRaisesRegex(RuntimeError, "exactly one"):
                            native.native_tests("/owned/tests", {})
                    self.assertEqual(run.call_count, 1)


if __name__ == "__main__":
    unittest.main()
