#!/usr/bin/env python3
"""TLS runner regressions using real libtest output, without a TLS server."""

import contextlib
import importlib.util
import io
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import patch


spec = importlib.util.spec_from_file_location(
    "transport", Path(__file__).with_name("live-transport.py")
)
transport = importlib.util.module_from_spec(spec)
spec.loader.exec_module(transport)


class CaseEvidence(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.directory = tempfile.TemporaryDirectory(prefix="pbps-tls-runner-")
        cls.addClassCleanup(cls.directory.cleanup)
        root = Path(cls.directory.name)
        source = root / "cases.rs"
        source.write_text("""
#[test] #[ignore] fn passes() {}
#[test] #[ignore] fn passes_with_the_same_prefix() {}
#[test] #[ignore] fn fails() { panic!("controlled failure"); }
#[test] fn not_a_fixture() {}
""")
        cls.binary = root / "cases"
        subprocess.run(["rustc", "--test", str(source), "-o", str(cls.binary)], check=True)

    def invoke(self, name):
        with contextlib.redirect_stdout(io.StringIO()) as output:
            transport.run_case([str(self.binary)], name, dict(os.environ))
        return output.getvalue()

    def test_exact_selection_passes_even_with_a_same_prefix_case(self):
        output = self.invoke("passes")
        self.assertIn("test passes ... ok", output)
        self.assertNotIn("test passes_with_the_same_prefix ...", output)

    def test_absent_filtered_and_failing_cases_are_not_success(self):
        for name in ("absent", "not_a_fixture", "fails"):
            with self.subTest(name=name):
                with self.assertRaisesRegex(RuntimeError, "exactly once"):
                    self.invoke(name)

    def test_the_real_zero_case_success_is_the_counterexample(self):
        result = subprocess.run(
            [str(self.binary), "--ignored", "--exact", "absent"],
            text=True, capture_output=True, check=True,
        )
        self.assertIn("test result: ok. 0 passed", result.stdout)
        with self.assertRaisesRegex(RuntimeError, "exactly once"):
            self.invoke("absent")

    def test_incomplete_wrong_or_ambiguous_evidence_is_refused(self):
        good = self.invoke("passes")
        summary = next(line for line in good.splitlines() if line.startswith("test result:"))
        cases = (
            (0, ""),
            (0, summary + "\n"),
            (0, "test passes ... ok\n"),
            (0, good.replace("test passes ... ok", "test renamed ... ok")),
            (0, good.replace("test passes ... ok", "test passes ... ignored")),
            (0, good.replace("0 ignored", "1 ignored")),
            (0, good + "test passes ... ok\n"),
            (0, good + summary + "\n"),
            (0, good.replace("1 passed", "2 passed")),
            (1, good),
        )
        for code, output in cases:
            with self.subTest(code=code, output=output):
                result = subprocess.CompletedProcess([], code, output)
                with patch.object(transport.subprocess, "run", return_value=result):
                    with contextlib.redirect_stdout(io.StringIO()):
                        with self.assertRaisesRegex(RuntimeError, "exactly once"):
                            transport.run_case([str(self.binary)], "passes", {})


class FixtureOwnership(unittest.TestCase):
    def test_every_trust_variant_uses_the_guard_and_every_exit_removes_the_fixture(self):
        verified = "verified_round_trips_reject_wrong_peers_and_corrupted_replies"
        invalid = "invalid_trust_cannot_yield_a_verified_connection"
        expected = [(verified, "ca.pem"), (invalid, "untrusted.pem"),
                    (invalid, "invalid.pem"), (invalid, "missing.pem")]
        for engine in ("pg", "mssql"):
            for fail_at in (None, 0, 1, 2, 3):
                with self.subTest(engine=engine, fail_at=fail_at):
                    calls = []

                    def run_case(command, name, env):
                        calls.append((name, Path(env["SSL_CERT_FILE"]).name))
                        self.assertEqual(env["PBPS_TEST_TLS_ENGINE"], engine)
                        self.assertEqual(command, ["cargo", "test", "-p", "pbps-db",
                                                   "--test", "live_transport", "--"])
                        if len(calls) - 1 == fail_at:
                            raise RuntimeError("controlled case refusal")

                    def run(*args, **kwargs):
                        port = "5432/tcp" if engine == "pg" else "1433/tcp"
                        return subprocess.CompletedProcess(
                            args, 0, json.dumps({port: [{"HostPort": "54321"}]}), ""
                        )

                    with (patch.object(transport, "run", side_effect=run) as commands,
                          patch.object(transport, "run_case", side_effect=run_case),
                          patch.object(transport.subprocess, "run",
                                       return_value=subprocess.CompletedProcess([], 0)),
                          patch.object(transport.signal, "signal"),
                          patch("sys.argv", ["live-transport.py", engine]),
                          contextlib.redirect_stdout(io.StringIO())):
                        if fail_at is None:
                            transport.main()
                        else:
                            with self.assertRaisesRegex(RuntimeError, "controlled case refusal"):
                                transport.main()
                    count = len(expected) if fail_at is None else fail_at + 1
                    self.assertEqual(calls, expected[:count])
                    created = next(call.args for call in commands.call_args_list
                                   if call.args[:2] == ("docker", "create"))
                    name = created[created.index("--name") + 1]
                    self.assertTrue(name.startswith("pbps-tls-"))
                    self.assertEqual(commands.call_args_list[-1].args,
                                     ("docker", "rm", "-fv", name))


if __name__ == "__main__":
    unittest.main()
