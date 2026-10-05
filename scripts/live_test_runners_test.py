#!/usr/bin/env python3
"""Regressions for live-test execution ownership, without starting fixtures."""

import contextlib
import importlib.util
import io
import json
import os
from pathlib import Path
import re
import shlex
import subprocess
import sys
import tempfile
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

    def test_ci_postgresql_health_checks_refuse_the_initialization_server(self):
        workflow = (ROOT / ".github/workflows/ci.yml").read_text()
        commands = [shlex.split(value) for value in
                    re.findall(r'--health-cmd "(pg_isready[^"\n]+)"', workflow)]
        self.assertEqual(len(commands), 2)
        for command in commands:
            with self.subTest(command=command):
                # Socket readiness is true while docker-entrypoint.sh is
                # initializing; the measured temporary server has no TCP.
                accepts_temporary = "-h" not in command or command[command.index("-h") + 1].startswith("/")
                self.assertFalse(accepts_temporary)
                self.assertEqual(command[command.index("-h") + 1], "127.0.0.1")

    def test_local_postgresql_deadlines_stop_before_any_client_suite(self):
        for mode, failing in (("deadline-new", "pbps-test-pg"),
                              ("deadline-old", "pbps-test-pg16")):
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
        for name in ("pbps-test-pg", "pbps-test-pg16"):
            probes = [c for c in calls[:first_client] if c[:3] == ["docker", "exec", name]]
            self.assertEqual(len(probes), 3)
        self.assertEqual([c for c in calls if c[0] in ("cargo", "python3")], [
            ["cargo", "test", "-p", "pbps-pg", "--test", "live", "--", "--ignored"],
            ["cargo", "test", "--profile", "live-test", "-p", "pbps-pg", "--lib", "--", "--ignored", "--test-threads=1"],
            ["cargo", "test", "-p", "pbps-db", "--test", "live_pg", "--", "--ignored"],
            ["python3", "scripts/live-transport.py", "pg"],
            ["cargo", "test", "-p", "pbps-cli", "--bin", "pbps", "--", "--ignored", "--test-threads=1"],
            ["cargo", "test", "-p", "pbps-cli", "--test", "flow_pg", "--", "--ignored", "--test-threads=1"],
        ])

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
    print("pbps-test-pg\npbps-test-pg16")
elif command[:2] == ["docker", "exec"]:
    name = command[2]
    state = root / name
    count = int(state.read_text()) + 1 if state.exists() else 1
    state.write_text(str(count))
    mode = os.environ["PBPS_ADMISSION_MODE"]
    tcp = "-h" in command and command[command.index("-h") + 1] == "127.0.0.1"
    deadline = (mode == "deadline-new" and name == "pbps-test-pg" or
                mode == "deadline-old" and name == "pbps-test-pg16")
    sys.exit(2 if tcp and (deadline or mode == "initializing" and count <= 2) else 0)
elif command[:2] == ["docker", "logs"]:
    print("controlled server still initializing", file=sys.stderr)
elif command[0] not in ("cargo", "python3", "sleep"):
    raise RuntimeError("Unexpected external command: " + repr(command))
''')
            fake.chmod(0o755)
            for name in ("docker", "cargo", "python3", "sleep"):
                (root / name).symlink_to(fake)
            env = dict(os.environ, PATH=str(root) + os.pathsep + os.environ["PATH"],
                       PBPS_ADMISSION_CONTROL=str(root), PBPS_ADMISSION_MODE=mode)
            result = subprocess.run(["bash", "scripts/live-tests-pg.sh"], cwd=ROOT,
                                    env=env, text=True, capture_output=True, timeout=30)
            calls = [json.loads(line) for line in (root / "calls").read_text().splitlines()]
            return result, calls

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
                native.DEADLINE_TEST,
                native.ADMIN_RECOVERY_TEST, native.SCRATCH_RECOVERY_TEST,
                native.JANITOR_RECOVERY_TEST,
                native.SQL_RECOVERY_CLOSE_TEST,
                native.SQL_RECOVERY_DISCARD_TEST,
                native.SQL_RECOVERY_CANCEL_TEST,
                *native.PRODUCER_TESTS,
            )
        ])
        for call in run.call_args_list:
            for key, value in env.items():
                self.assertEqual(call.kwargs["env"][key], value)

    def test_focused_pg_producer_selector_keeps_existing_native_cases_out(self):
        completed = subprocess.CompletedProcess([], 0, "test result: ok. 1 passed\n")
        with patch.object(native, "run", return_value=completed) as run:
            with contextlib.redirect_stdout(io.StringIO()):
                native.native_tests("/owned/tests", {"PBPS_NATIVE_DRIVER": "pg"}, producer_only=True)
        self.assertEqual([call.args[3] for call in run.call_args_list], native.PRODUCER_TESTS)
        self.assertEqual(run.call_count, 37)

    def test_focused_generation_runs_only_the_case_for_the_selected_pg_major(self):
        completed = subprocess.CompletedProcess([], 0, "test result: ok. 1 passed\n")
        for major, selected, opposite in (
            ("16", native.PG16_GENERATION_TEST, native.PG18_GENERATION_TEST),
            ("18", native.PG18_GENERATION_TEST, native.PG16_GENERATION_TEST),
        ):
            with self.subTest(major=major):
                env = {"PBPS_NATIVE_DRIVER": "pg", "PBPS_NATIVE_PG_MAJOR": major}
                with patch.object(native, "run", return_value=completed) as run:
                    with contextlib.redirect_stdout(io.StringIO()):
                        native.native_tests("/owned/tests", env, generation_only=True)
                self.assertEqual([call.args for call in run.call_args_list],
                                 [("/owned/tests", "--ignored", "--exact", selected, "--nocapture")])
                self.assertNotIn(opposite, [call.args[3] for call in run.call_args_list])
                self.assertEqual(run.call_args.kwargs["env"]["PBPS_NATIVE_PG_MAJOR"], major)

    def test_normal_pg_execution_keeps_all_old_cases_and_only_its_generation_case(self):
        completed = subprocess.CompletedProcess([], 0, "test result: ok. 1 passed\n")
        every = native.NATIVE_TESTS + native.PRODUCER_TESTS + native.GENERATION_TESTS
        self.assertEqual(len(every), 53)
        for major, selected, opposite in (
            ("16", native.PG16_GENERATION_TEST, native.PG18_GENERATION_TEST),
            ("18", native.PG18_GENERATION_TEST, native.PG16_GENERATION_TEST),
        ):
            with self.subTest(major=major):
                env = {"PBPS_NATIVE_DRIVER": "pg", "PBPS_NATIVE_PG_MAJOR": major}
                with patch.object(native, "run", return_value=completed) as run:
                    with contextlib.redirect_stdout(io.StringIO()):
                        native.native_tests("/owned/tests", env)
                names = [call.args[3] for call in run.call_args_list]
                self.assertEqual(names, every[:-2] + [selected])
                self.assertEqual(len(names), 52)
                self.assertNotIn(opposite, names)

    def test_focused_generation_refuses_missing_or_unknown_major_and_non_pg(self):
        for env, message in (
            ({"PBPS_NATIVE_DRIVER": "pg"}, "PBPS_NATIVE_PG_MAJOR"),
            ({"PBPS_NATIVE_DRIVER": "pg", "PBPS_NATIVE_PG_MAJOR": ""}, "PBPS_NATIVE_PG_MAJOR"),
            ({"PBPS_NATIVE_DRIVER": "pg", "PBPS_NATIVE_PG_MAJOR": "17"}, "PBPS_NATIVE_PG_MAJOR"),
            ({"PBPS_NATIVE_DRIVER": "mssql", "PBPS_NATIVE_PG_MAJOR": "16"}, "PBPS_NATIVE_DRIVER"),
            ({"PBPS_NATIVE_PG_MAJOR": "18"}, "PBPS_NATIVE_DRIVER"),
        ):
            with self.subTest(env=env):
                with patch.object(native, "run") as run:
                    with self.assertRaisesRegex(RuntimeError, message):
                        native.native_tests("/owned/tests", env, generation_only=True)
                run.assert_not_called()

    def test_native_focused_selectors_refuse_overlap_before_execution(self):
        with patch.object(native, "run") as run:
            with self.assertRaisesRegex(RuntimeError, "mutually exclusive"):
                native.native_tests("/owned/tests", {"PBPS_NATIVE_DRIVER": "pg"},
                                    producer_only=True, generation_only=True)
        run.assert_not_called()

    def test_generation_cli_refuses_non_native_pg_and_overlapping_modes_before_fixture(self):
        for arguments, message in (
            (["pg", "--generation-only"], "--generation-only requires --native-host pg"),
            (["mssql", "--native-host", "--generation-only"], "--generation-only requires --native-host pg"),
            (["pg", "--native-host", "--producer-only", "--generation-only"], "not allowed with argument"),
        ):
            with self.subTest(arguments=arguments):
                stderr = io.StringIO()
                with patch.object(native.sys, "argv", ["native-target", *arguments]):
                    with patch.object(native, "fixture") as fixture, patch.object(native, "test_binary") as binary:
                        with contextlib.redirect_stderr(stderr):
                            with self.assertRaises(SystemExit) as error:
                                native.main()
                        self.assertEqual(error.exception.code, 2)
                        fixture.assert_not_called()
                        binary.assert_not_called()
                self.assertIn(message, stderr.getvalue())

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
