#!/usr/bin/env python3
"""Tests for the startup-crash retry in scripts/live-resolver-server.py (#958).
Run: python3 scripts/live_resolver_server_test.py

The fixture itself needs root and a Docker daemon, and runs only in CI. What
is tested here is the decision it makes about a failed start, against a fake
`docker`: that decision is where a real failure could be retried into a pass.
"""

import importlib.util
import subprocess
import unittest
from pathlib import Path

spec = importlib.util.spec_from_file_location(
    "live_resolver_server", Path(__file__).with_name("live-resolver-server.py")
)
server = importlib.util.module_from_spec(spec)
spec.loader.exec_module(server)

DUMP = ("Capture info: Current application configuration\n"
        "Dump already generated: /var/opt/mssql/log/core.sqlservr.9_25_2026_16_55_13.18, "
        "moving to /var/opt/mssql/log/core.sqlservr.18.temp/core.sqlservr.18.gdmp\n")


class FakeDocker:
    """Answers `docker inspect` and `docker logs` from a script of starts:
    each start is (running, log), and the container shows the latest one."""

    def __init__(self, starts):
        self.starts = list(starts)
        self.current = self.starts.pop(0)
        self.calls = []

    def run(self, *args, **kwargs):
        self.calls.append(args)
        verb = args[1]
        if verb == "inspect":
            return subprocess.CompletedProcess(args, 0, "true" if self.current[0] else "false", "")
        if verb == "logs":
            return subprocess.CompletedProcess(args, 0, self.current[1], "")
        return subprocess.CompletedProcess(args, 0, "", "")

    def restart(self):
        self.current = self.starts.pop(0)

    def await_engine(self, container, engine):
        if not self.current[0]:
            raise RuntimeError(f"owned fixture {container} did not become ready")


class AFailedStart(unittest.TestCase):
    def ready(self, engine, starts):
        docker = FakeDocker(starts)
        restarts = []

        def start():
            restarts.append(1)
            docker.restart()

        server.run, server.await_engine = docker.run, docker.await_engine
        try:
            server.ready("pbps-dedicated-target-x", engine, start)
        finally:
            server.run, server.await_engine = self.real
        return len(restarts)

    def setUp(self):
        self.real = (server.run, server.await_engine)

    def test_a_startup_core_dump_is_started_once_more(self):
        self.assertEqual(self.ready("mssql", [(False, DUMP), (True, "")]), 1)

    def test_a_second_core_dump_fails_the_run(self):
        with self.assertRaisesRegex(RuntimeError, "did not become ready"):
            self.ready("mssql", [(False, DUMP), (False, DUMP), (True, "")])

    def test_an_exit_without_a_dump_is_not_retried(self):
        with self.assertRaisesRegex(RuntimeError, "did not become ready"):
            self.ready("mssql", [(False, "Login failed for user 'sa'.\n"), (True, "")])

    def test_an_engine_still_running_is_not_retried(self):
        # It never answered, but it did not crash: that is a failure to read,
        # not the transient this retries.
        docker = FakeDocker([(True, DUMP), (True, "")])

        def never(container, engine):
            raise RuntimeError(f"owned fixture {container} did not become ready")

        server.run, server.await_engine = docker.run, never
        try:
            with self.assertRaisesRegex(RuntimeError, "did not become ready"):
                server.ready("pbps-dedicated-target-x", "mssql", docker.restart)
        finally:
            server.run, server.await_engine = self.real
        self.assertEqual(len(docker.starts), 1, "restarted a running engine")

    def test_postgresql_is_never_retried(self):
        with self.assertRaisesRegex(RuntimeError, "did not become ready"):
            self.ready("pg", [(False, DUMP), (True, "")])


class TheRunningProbe(unittest.TestCase):
    def test_an_unreadable_state_keeps_the_wait_going(self):
        real = server.run
        server.run = lambda *a, **k: subprocess.CompletedProcess(a, 1, "", "no such object")
        try:
            self.assertTrue(server.running("gone"))
        finally:
            server.run = real

    def test_an_exited_container_is_not_running(self):
        real = server.run
        server.run = lambda *a, **k: subprocess.CompletedProcess(a, 0, "false\n", "")
        try:
            self.assertFalse(server.running("exited"))
        finally:
            server.run = real


if __name__ == "__main__":
    unittest.main()
