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
from contextlib import ExitStack
from unittest import mock
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


STORAGE_CASE = "resolver::server::live_tests::pg16_storage::the_supplied_storage_layout_admits_its_observed_major_and_survives_live_checks"
LEGACY_CASE = "resolver::server::live_tests::a_supported_dedicated_server_compiles_declarations_and_removes_only_its_own_resources"
PG_IMAGES = {
    16: "postgres@sha256:485935f94cc7165afa896978809c37b592dc07f0a37d2c8f645f12412d0212c8",
    18: "postgres@sha256:4ef4dbc939d61acea57712655ddb4b4ab27419c913f94cca0cd57cb3ea3c2280",
}


class TheFixedStorageRecipe(unittest.TestCase):
    """Observe maintained command assembly, never create or qualify a container."""

    def commands(self, engine, major=None):
        commands = []

        def recorded(*args, **kwargs):
            commands.append((args, kwargs))
            output = "test result: ok. 1 passed; 0 failed; 0 ignored\n"
            return subprocess.CompletedProcess(args, 0, output, "")

        argv = ["live-resolver-server.py", engine, "--test-binary", "/unit/pbps-cli"]
        if major is not None:
            argv += ["--pg-major", str(major)]
        with ExitStack() as stack:
            stack.enter_context(mock.patch("sys.argv", argv))
            stack.enter_context(mock.patch.object(server.os, "geteuid", return_value=0))
            stack.enter_context(mock.patch.object(server, "ENGINE", None, create=True))
            stack.enter_context(mock.patch.object(server, "DOCKER_SOCKET", server.DOCKER_SOCKET))
            stack.enter_context(mock.patch.object(server, "run", side_effect=recorded))
            stack.enter_context(mock.patch.object(server.subprocess, "run", return_value=subprocess.CompletedProcess([], 0)))
            stack.enter_context(mock.patch.object(Path, "mkdir"))
            stack.enter_context(mock.patch.object(Path, "write_text"))
            for name in ("certificates", "started", "ready", "describe", "statement"):
                stack.enter_context(mock.patch.object(server, name))
            stack.enter_context(mock.patch.object(server, "service_pid", return_value="123"))
            stack.enter_context(mock.patch.object(server, "TESTS", [LEGACY_CASE, STORAGE_CASE]))
            # This recipe oracle stays focused when other owners add cases.
            stack.enter_context(mock.patch.object(server, "PRODUCER_TESTS", [], create=True))
            server.main()
        return commands

    def supplied_create(self, commands):
        selected = [args for args, _ in commands if args[:2] == ("docker", "create")
                    and args[args.index("--name") + 1].startswith("pbps-dedicated-server-")]
        self.assertEqual(len(selected), 1, "one actual supplied-create command is expected")
        return selected[0]

    def test_pg16_and_pg18_route_their_fixed_recipes_to_the_actual_storage_case(self):
        self.assertIn(STORAGE_CASE, server.TESTS, "the native owner must select this exact case")
        for major, storage, profile in [
            (16, "/var/lib/postgresql/data", "linux-dedicated-pg16-v1"),
            (18, "/var/lib/postgresql", "linux-dedicated-v1"),
        ]:
            with self.subTest(major=major):
                commands = self.commands("pg", major)
                create = self.supplied_create(commands)
                self.assertIn(PG_IMAGES[major], create)
                self.assertEqual(create[create.index("--user") + 1], "999:999")
                mounts = [create[i + 1] for i, value in enumerate(create) if value == "--tmpfs"]
                storage_mounts = [value for value in mounts if value.startswith("/var/lib/postgresql")]
                self.assertEqual(storage_mounts, [storage + ":rw,nosuid,nodev,noexec,size=268435456,uid=999,gid=999,mode=700"])
                boot = create[-1]
                self.assertIn(f"/usr/lib/postgresql/{major}/bin/initdb", boot)
                self.assertIn(f"exec /usr/lib/postgresql/{major}/bin/postgres", boot)
                self.assertIn(f"-D {storage}/run-data", boot)
                self.assertIn(f"{storage}/pw", boot)
                self.assertNotIn(f"/usr/lib/postgresql/{18 if major == 16 else 16}/bin/", boot)
                selected = [(args, kwargs) for args, kwargs in commands if "--exact" in args]
                self.assertEqual([args[args.index("--exact") + 1] for args, _ in selected], [
                    LEGACY_CASE,
                    STORAGE_CASE,
                ])
                for _, kwargs in selected:
                    self.assertEqual(kwargs["env"]["PBPS_SERVER_PG_MAJOR"], str(major))
                    fields = dict(part.split("=", 1) for part in kwargs["env"]["PBPS_SERVER_ENDPOINT"].split())
                    self.assertEqual(fields["profile"], profile)
                    self.assertTrue(fields["container"].startswith("pbps-dedicated-server-"))

    def test_pg18_remains_the_default_and_sql_server_keeps_its_old_case(self):
        default = self.supplied_create(self.commands("pg"))
        self.assertIn(PG_IMAGES[18], default)
        self.assertIn("/usr/lib/postgresql/18/bin/initdb", default[-1])
        commands = self.commands("mssql")
        create = self.supplied_create(commands)
        self.assertIn("mcr.microsoft.com/mssql/server@sha256:4bab24f36c1ecd48e85f7d37df26e6bf301641d84c3fe652f9a0dcc947d512e1", create)
        self.assertIn("/var/opt/mssql:rw,nosuid,nodev,noexec,size=1073741824,uid=10001,gid=0,mode=700", create)
        self.assertIn("exec /opt/mssql/bin/launch_sqlservr.sh /opt/mssql/bin/sqlservr", create[-1])
        self.assertNotIn("initdb", create[-1])
        selected = [(args, kwargs) for args, kwargs in commands if "--exact" in args]
        self.assertEqual([args[args.index("--exact") + 1] for args, _ in selected], [
            LEGACY_CASE,
        ])
        self.assertIn("profile=linux-dedicated-v1 ", selected[0][1]["env"]["PBPS_SERVER_ENDPOINT"])

    def test_unsupported_majors_and_sql_server_pg_overrides_create_nothing(self):
        for engine, major in [("pg", 17), ("mssql", 16)]:
            with self.subTest(engine=engine, major=major):
                with mock.patch.object(server, "fixture") as fixture_call:
                    with self.assertRaises(SystemExit):
                        self.commands(engine, major)
                fixture_call.assert_not_called()


if __name__ == "__main__":
    unittest.main()
