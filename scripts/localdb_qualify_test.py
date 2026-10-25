#!/usr/bin/env python3
"""Tests for scripts/localdb_qualify.py. Run: python3 scripts/localdb_qualify_test.py

The script itself runs only on a Windows runner; these are the parts that
decide whether a case ran and what the user's process is handed.
"""

import importlib.util
import json
import re
import string
import sys
import unittest
from pathlib import Path

SCRIPT = Path(__file__).with_name("localdb_qualify.py")
spec = importlib.util.spec_from_file_location("localdb_qualify", SCRIPT)
lq = importlib.util.module_from_spec(spec)
spec.loader.exec_module(lq)

OK = "test result: ok. {} passed; 0 failed; 0 ignored; 0 measured; 3 filtered out; finished in 1.00s"


class Counting(unittest.TestCase):
    def test_a_run_must_pass_exactly_the_cases_it_listed(self):
        lq.require_ran(OK.format(4), 4, "suite")

    def test_zero_selected_cases_is_not_a_pass(self):
        # libtest exits 0 for a filter that matches nothing.
        with self.assertRaisesRegex(RuntimeError, "no case was selected"):
            lq.require_ran(OK.format(0), 0, "suite")

    def test_fewer_or_more_passes_than_listed_fail(self):
        for passed in (3, 5):
            with self.assertRaises(RuntimeError):
                lq.require_ran(OK.format(passed), 4, "suite")

    def test_a_failure_or_a_missing_or_doubled_summary_fails(self):
        bad = "test result: FAILED. 3 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out"
        for output in (bad, "", OK.format(1) + "\n" + OK.format(1)):
            with self.assertRaises(RuntimeError):
                lq.require_ran(output, 1, "suite")

    def test_a_named_case_must_be_the_only_one_and_must_pass(self):
        output = "test a::b ... ok\n" + OK.format(1)
        lq.require_case(output, "a::b")
        for wrong in ("test a::c ... ok\n" + OK.format(1),
                      "test a::b ... FAILED\n" + OK.format(1),
                      "test a::b ... ok\ntest a::c ... ok\n" + OK.format(2),
                      OK.format(1)):
            with self.assertRaises(RuntimeError):
                lq.require_case(wrong, "a::b")

    def test_a_listing_counts_cases_only(self):
        self.assertEqual(lq.listed("a::x: test\nb::y: test\n\n2 tests, 0 benchmarks\n"), ["a::x", "b::y"])
        self.assertEqual(lq.listed("0 tests, 0 benchmarks\n"), [])


class Handoff(unittest.TestCase):
    def test_the_wrapper_sets_the_environment_and_redirects_output(self):
        text = lq.wrapper({"B": r"Server=np:\\.\pipe\x;a=(b)", "A": "1"}, [r"C:\t\x.exe", "--ignored", "n"],
                          r"C:\o\out.log")
        self.assertEqual(text.splitlines()[0], "@echo off")
        self.assertLess(text.index('set "A=1"'), text.index('set "B='))
        self.assertIn(r'"C:\t\x.exe" "--ignored" "n" > "C:\o\out.log" 2>&1', text)
        self.assertTrue(text.endswith("exit /b %ERRORLEVEL%\r\n"))
        split = lq.wrapper({}, ["x"], "o", "e")
        self.assertIn('> "o" 2> "e"', split)

    def test_values_cmd_would_reinterpret_are_refused_not_escaped(self):
        for bad in ('a%PATH%', "a^b", 'a"b', "a!b", "a&calc", "a|b", "a<b", "a>b", "a\nb"):
            with self.assertRaises(ValueError, msg=bad):
                lq.wrapper({"K": bad}, ["x"], "o")
            with self.assertRaises(ValueError, msg=bad):
                lq.wrapper({}, ["x", bad], "o")

    def test_the_selector_is_the_integrated_pipe_form_the_driver_admits(self):
        text = lq.selector("pbps_scratch")
        self.assertEqual(text, r"Server=(localdb)\pbps_scratch;Integrated Security=SSPI;Encrypt=DANGER_PLAINTEXT")
        lq.wrapper({"S": text}, ["x"], "o")  # survives the wrapper unchanged
        for name in (lq.TARGET, lq.SCRATCH, lq.DISPOSABLE):
            self.assertRegex(name, r"[A-Za-z0-9_]{1,128}")

    def test_the_password_meets_windows_complexity_and_is_fresh(self):
        seen = {lq.password() for _ in range(20)}
        self.assertEqual(len(seen), 20)
        for secret in seen:
            self.assertGreaterEqual(len(secret), 24)
            for pool in (string.ascii_lowercase, string.ascii_uppercase, string.digits):
                self.assertTrue(set(secret) & set(pool), secret)
            self.assertFalse(set(secret) & lq.UNSAFE)

    def test_the_user_is_created_without_a_console_prompt_or_the_password_in_argv(self):
        # `net user /add` asks for confirmation of a password over 14
        # characters and fails with no console; the password comes from the
        # environment, never the command text.
        command = lq.create_user_command()
        self.assertNotIn("net user", command)
        self.assertIn("New-LocalUser", command)
        self.assertIn("$env:PBPS_LOCALDB_PASSWORD", command)
        self.assertIn("S-1-5-32-545", command)
        self.assertNotIn("Administrators", command)
        self.assertNotIn("S-1-5-32-544", command)

    def test_the_fixture_key_is_a_fresh_key_of_the_generated_size(self):
        import base64
        keys = {lq.fingerprint_key() for _ in range(10)}
        self.assertEqual(len(keys), 10)
        for key in keys:
            self.assertEqual(len(base64.b64decode(key, validate=True)), 32)
            lq.wrapper({"K": key}, ["x"], "o")  # survives the wrapper unchanged

    def test_windows_powershell_children_do_not_inherit_powershell_7s_module_path(self):
        # PowerShell 7 runs the step; its PSModulePath makes Windows
        # PowerShell 5.1 fail to load its own built-in modules.
        base = {"PSModulePath": r"C:\pwsh7\Modules", "Path": r"C:\Windows", "psmodulepath": "x"}
        env = lq.windows_powershell_env("s3cret", base)
        self.assertNotIn("PSMODULEPATH", {k.upper() for k in env})
        self.assertEqual(env["Path"], r"C:\Windows")
        self.assertEqual(env["PBPS_LOCALDB_PASSWORD"], "s3cret")
        self.assertIn("PSModulePath", base)  # the caller's mapping is left alone


class Pinned(unittest.TestCase):
    def test_the_installer_is_pinned_by_content_on_microsoft_downloads(self):
        self.assertRegex(lq.MSI_SHA256, r"[0-9a-f]{64}")
        self.assertTrue(lq.MSI_URL.startswith("https://download.microsoft.com/download/"))
        self.assertTrue(lq.MSI_URL.endswith("/SqlLocalDB.msi"))

    def test_the_helper_path_is_the_qualified_majors_directory(self):
        self.assertEqual(lq.HELPER.parts[-4:], ("170", "Tools", "Binn", "SqlLocalDB.exe"))
        # The resolver looks in the same place.
        source = Path(__file__).resolve().parents[1] / "crates/pbps-mssql/src/resolver/localdb.rs"
        self.assertIn('const QUALIFIED_DIRECTORY: &str = "170";', source.read_text(encoding="utf-8"))


def artifact(source, kind, name, executable, test=True):
    return json.dumps({"reason": "compiler-artifact", "executable": executable,
                       "profile": {"test": test}, "target": {"src_path": source, "kind": [kind], "name": name}})


class Executables(unittest.TestCase):
    def test_exactly_one_executable_of_the_package_and_kind_is_taken(self):
        messages = "\n".join([
            "not json",
            artifact("C:\\w\\crates\\pbps-mssql\\tests\\live.rs", "test", "live", "live.exe"),
            artifact("C:\\w\\crates\\pbps-mssql\\tests\\slots.rs", "test", "slots", "slots.exe"),
            artifact("C:\\w\\crates\\pbps-cli\\src\\lib.rs", "lib", "pbps_cli", "cli.exe"),
            artifact("C:\\w\\crates\\pbps-mssql\\examples\\localdb_endpoint.rs", "example", "localdb_endpoint", "ex.exe", False),
            json.dumps({"reason": "compiler-artifact", "executable": None, "profile": {"test": False},
                        "target": {"src_path": "/w/crates/pbps-cli/src/lib.rs", "kind": ["lib"], "name": "pbps_cli"}}),
        ])
        self.assertEqual(lq.executable(messages, "pbps-mssql", "test", "live"), "live.exe")
        self.assertEqual(lq.executable(messages, "pbps-cli", "lib"), "cli.exe")
        self.assertEqual(lq.executable(messages, "pbps-mssql", "example", "localdb_endpoint"), "ex.exe")

    def test_none_or_several_is_an_error(self):
        one = artifact("/w/crates/pbps-cli/src/lib.rs", "lib", "pbps_cli", "a")
        with self.assertRaises(RuntimeError):
            lq.executable("", "pbps-cli", "lib")
        with self.assertRaises(RuntimeError):
            lq.executable(one + "\n" + one, "pbps-cli", "lib")
        with self.assertRaises(RuntimeError):
            lq.executable(one, "pbps-mssql", "lib")


class Plan(unittest.TestCase):
    def source(self):
        return SCRIPT.read_text(encoding="utf-8")

    def test_every_localdb_case_the_script_runs_is_defined_in_the_live_tests(self):
        tests = (Path(__file__).resolve().parents[1]
                 / "crates/pbps-cli/src/resolver/server/scratch/localdb_live_tests.rs").read_text(encoding="utf-8")
        defined = set(re.findall(r"async fn (localdb2271_\w+)\(", tests.replace("\n ", " ")))
        run = set(re.findall(r'run_case\(cli, "resolver::server::scratch::localdb_live_tests::(\w+)"\)', self.source()))
        self.assertTrue(run)
        self.assertEqual(run, defined, "a defined case the job never runs, or a run case that is gone")

    def test_every_shared_case_exists_and_none_uses_a_sql_login_or_docker(self):
        tests = (Path(__file__).resolve().parents[1]
                 / "crates/pbps-cli/src/resolver/server/scratch/mssql_live_tests.rs").read_text(encoding="utf-8")
        for case in lq.SHARED_CLI:
            body = re.search(rf"async fn {case}\(\) \{{(.*?)\n\}}\n", tests, re.S)
            self.assertIsNotNone(body, case)
            for needle in ("User Id", "CREATE LOGIN", "with_container", "supplied_contract"):
                self.assertNotIn(needle, body.group(1), f"{case} needs {needle}, which a pipe cannot offer")


if __name__ == "__main__":
    unittest.main()
