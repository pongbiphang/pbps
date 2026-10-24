#!/usr/bin/env python3
"""Regressions for live-test execution ownership, without starting fixtures."""

import json
import os
from pathlib import Path
import re
import shlex
import subprocess
import sys
import tempfile
import textwrap
import unittest


ROOT = Path(__file__).resolve().parents[1]


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
                        env = dict(os.environ,
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
            env = dict(os.environ, PATH=str(root) + os.pathsep + os.environ['PATH'],
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
            env = dict(os.environ, PATH=str(root) + os.pathsep + os.environ["PATH"],
                       PBPS_ADMISSION_CONTROL=str(root), PBPS_ADMISSION_MODE=mode)
            result = subprocess.run(["bash", "scripts/live-tests-pg.sh"], cwd=ROOT,
                                    env=env, text=True, capture_output=True, timeout=30)
            calls = [json.loads(line) for line in (root / "calls").read_text().splitlines()]
            return result, calls

if __name__ == "__main__":
    unittest.main()
