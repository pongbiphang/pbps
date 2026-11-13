#!/usr/bin/env python3
"""Tests for scripts/pgwin_qualify.py. Run: python3 scripts/pgwin_qualify_test.py

The script runs only on a Windows runner; these are the parts that decide what
it installs, how a cluster is closed, which cases it must run and whether the
registry, the wrappers and the script agree, checked on every platform.
"""

import ast
import importlib.util
import json
import re
import sys
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
ROOT = HERE.parent
sys.path.insert(0, str(HERE))
SCRIPT = HERE / "pgwin_qualify.py"
spec = importlib.util.spec_from_file_location("pgwin_qualify", SCRIPT)
pq = importlib.util.module_from_spec(spec)
spec.loader.exec_module(pq)

WRAPPERS = ROOT / "crates/pbps-cli/tests/support/pgwin2491.rs"


def source():
    return SCRIPT.read_text(encoding="utf-8")


def selected_cases():
    main = next(n for n in ast.parse(source()).body if isinstance(n, ast.FunctionDef) and n.name == "main")
    return [node.args[1].value for node in ast.walk(main)
            if isinstance(node, ast.Call) and isinstance(node.func, ast.Name)
            and node.func.id == "run_case" and len(node.args) > 1 and isinstance(node.args[1], ast.Constant)]


class Pinned(unittest.TestCase):
    def test_both_supported_majors_are_pinned_by_content_on_the_vendors_download_host(self):
        self.assertEqual(sorted(pq.PINNED), [16, 18], "the resolver reads 16 and 18 and refuses any other major")
        for major, pin in pq.PINNED.items():
            self.assertRegex(pin["url"], rf"^https://get\.enterprisedb\.com/postgresql/postgresql-{major}\.\d+-\d+-windows-x64-binaries\.zip$")
            self.assertRegex(pin["sha256"], r"^[0-9a-f]{64}$")

    def test_the_build_number_agrees_with_the_archive_it_names(self):
        for major, pin in pq.PINNED.items():
            minor = int(re.search(rf"postgresql-{major}\.(\d+)-", pin["url"]).group(1))
            self.assertEqual(pin["version_num"], f"{major}{minor:04d}")

    def test_a_cluster_must_report_exactly_the_pinned_build(self):
        self.assertEqual(pq.require_build("160015\n", 16), "160015")
        for wrong in ("170011", "160014", "180006", "", "16.15"):
            with self.assertRaises(RuntimeError):
                pq.require_build(wrong, 16)
        with self.assertRaises(RuntimeError):
            pq.require_build("160015", 18)

    def test_the_hosted_images_own_postgresql_17_is_never_used(self):
        self.assertNotIn("PGBIN", source())
        self.assertNotIn(17, pq.PINNED)

    def test_the_archives_live_under_one_layout(self):
        self.assertEqual(pq.archive(18).name, "postgresql-18.6-2-windows-x64-binaries.zip")
        self.assertEqual(pq.pgbin(16, "initdb.exe").parts[-4:], ("pg16", "pgsql", "bin", "initdb.exe"))


class Clusters(unittest.TestCase):
    def test_two_servers_of_the_newer_major_and_the_older_one_each_on_a_port_of_its_own(self):
        self.assertEqual({name: major for name, (major, _) in pq.CLUSTERS.items()},
                         {"target": 18, "scratch": 18, "old": 16})
        ports = [port for _, port in pq.CLUSTERS.values()]
        self.assertEqual(len(ports), len(set(ports)))

    def test_every_cluster_is_closed_to_scram_on_the_loopback_only(self):
        lines = [line.split() for line in pq.SCRAM_HBA.strip().splitlines()]
        self.assertTrue(lines)
        for line in lines:
            self.assertEqual((line[0], line[-1]), ("host", "scram-sha-256"))
            self.assertIn(line[3], ("127.0.0.1/32", "::1/128"))
        self.assertNotIn("trust", pq.SCRAM_HBA)

    def test_the_password_goes_into_a_file_and_the_environment_never_a_command_line(self):
        password = "p" * 32
        sql = pq.bootstrap_sql(password)
        self.assertIn(password, sql)
        self.assertIn(f"CREATE DATABASE {pq.DATABASE}", sql)
        for major, port in ((16, 55443), (18, 55441)):
            argv = [str(part) for part in pq.psql_argv(major, port, pq.DATABASE)]
            self.assertNotIn(password, " ".join(argv))
            self.assertIn("-w", argv, "never prompt")
        self.assertNotIn(password, pq.server_arguments(55441))
        # The only place the script hands a password to a child is its environment.
        self.assertEqual(re.findall(r'env=\{"PGPASSWORD": passwords\[name\]\}', source()),
                         ['env={"PGPASSWORD": passwords[name]}'])
        self.assertNotIn("PGPASSFILE=", source().replace("(`PGPASSFILE`)", ""))

    def test_the_connection_strings_are_the_ones_the_linux_fixtures_spell(self):
        text = pq.connection(55441, "secret")
        for part in ("host=127.0.0.1", "port=55441", "user=postgres", "password=secret",
                     f"dbname={pq.DATABASE}", "sslmode=disable"):
            self.assertIn(part, text.split())

    def test_the_cases_get_one_connection_per_cluster_and_the_environment_key(self):
        env = pq.environment({name: f"pw-{name}" for name in pq.CLUSTERS})
        self.assertEqual(sorted(env), ["PBPS_TEST_FINGERPRINT_KEY", "PBPS_TEST_PG_DB", "PBPS_TEST_PG_OLD_DB",
                                       "PBPS_TEST_PG_SCRATCH_DB", "RUST_MIN_STACK"])
        self.assertIn("port=55441", env["PBPS_TEST_PG_DB"])
        self.assertIn("port=55442", env["PBPS_TEST_PG_SCRATCH_DB"])
        self.assertIn("port=55443", env["PBPS_TEST_PG_OLD_DB"])
        self.assertEqual(len(set(env[k] for k in ("PBPS_TEST_PG_DB", "PBPS_TEST_PG_SCRATCH_DB", "PBPS_TEST_PG_OLD_DB"))), 3)


class Plan(unittest.TestCase):
    def test_every_wrapper_is_run_exactly_once_and_nothing_else(self):
        tests = WRAPPERS.read_text(encoding="utf-8")
        defined = re.findall(r"\nfn (pg_\w+)\(\) \{", tests)
        run = selected_cases()
        self.assertTrue(defined)
        self.assertEqual(len(defined), len(set(defined)))
        self.assertEqual(run, [f"pgwin2491::{name}" for name in defined],
                         "a wrapper the job never runs, or a run case that is gone")
        # Every wrapper is an ignored test with the reason the Linux jobs' tests give.
        self.assertEqual(tests.count("#[ignore ="), len(defined))
        self.assertEqual(tests.count("#[test]"), len(defined))

    def test_the_docker_kind_is_never_asked_for_on_hosted_windows(self):
        tests = WRAPPERS.read_text(encoding="utf-8")
        self.assertNotIn("Kind::Docker", tests)
        self.assertNotIn("docker", re.sub(r"//!.*|//.*", "", tests).lower())

    def test_every_case_is_held_to_running_and_passing_exactly_once(self):
        text = source()
        self.assertIn("lq.require_case(text, test)", text)
        self.assertIn('"--exact"', text)
        self.assertIn("no qualification case ran", text)

    def test_the_owner_entries_are_exactly_the_cases_the_script_selects(self):
        """What the inventory checks only on Windows, checked here on every platform."""
        registry = json.loads((ROOT / "scripts/ignored-test-owners.json").read_text(encoding="utf-8"))
        owner = registry["owners"]["pgwin"]
        selection = owner["selection"]
        self.assertEqual((owner["platform"], owner["job"], owner["script"], selection["kind"], selection["file"],
                          selection["scope"], selection["callee"], selection["argument"]),
                         ("win32", "resolver-pg-windows", "scripts/pgwin_qualify.py", "calls",
                          "scripts/pgwin_qualify.py", "main", "run_case", 1))
        groups = [g for g in registry["groups"] if g["owner"] == "pgwin"]
        self.assertEqual([tuple(g["target"]) for g in groups], [("pbps-cli", "test", "flow_pg")])
        cases = [case for g in groups for case in g["cases"]]
        self.assertEqual(sorted(cases), sorted(selected_cases()))
        self.assertEqual(len(cases), len(set(cases)))
        for group in groups:
            self.assertEqual((group["validate_on"], group["ignored_on"]), ("win32", ["win32"]))

    def test_the_job_runs_this_script_and_the_gate_waits_for_it(self):
        workflow = (ROOT / ".github/workflows/ci.yml").read_text(encoding="utf-8")
        block = workflow.split("  resolver-pg-windows:\n")[1].split("\n\n")[0]
        self.assertIn("runs-on: windows-2025", block)
        self.assertIn("- run: python3 scripts/pgwin_qualify.py", block)
        self.assertRegex(workflow.split("  ci-gate:")[1] if "  ci-gate:" in workflow else workflow,
                         r"needs: \[[^\]]*\bresolver-pg-windows\b")


if __name__ == "__main__":
    unittest.main()
