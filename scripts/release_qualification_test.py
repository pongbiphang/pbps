#!/usr/bin/env python3
"""Negative controls for artifact qualification's fail-closed reporting."""

import importlib.util
from pathlib import Path
import sys
from types import SimpleNamespace
import unittest


SPEC = importlib.util.spec_from_file_location("release_qualification", Path(__file__).with_name("qualify-release.py"))
release = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(release)


class Qualification(unittest.TestCase):
    def test_missing_duplicate_or_out_of_order_cases_cannot_publish_evidence(self):
        complete = {engine: {"cases": list(release.CASES)} for engine in release.IMAGES}
        release.require_complete(complete)
        for cases in ([], list(release.CASES[:-1]), list(reversed(release.CASES)), [*release.CASES, release.CASES[-1]]):
            with self.subTest(cases=cases), self.assertRaises(RuntimeError):
                release.require_complete({**complete, "postgres": {"cases": cases}})
        for engines in ({}, {"postgres": complete["postgres"]}, {**complete, "other": complete["postgres"]}):
            with self.subTest(engines=engines), self.assertRaises(RuntimeError):
                release.require_complete(engines)

    def test_unreachable_server_and_authentication_errors_are_not_tls_qualification(self):
        for message in ("connection refused", "TLS connection timed out", "login failed", "executable missing"):
            with self.subTest(message=message), self.assertRaises(RuntimeError):
                release.require_tls_refusal(1, {"result": "unanswerable", "findings": [{"message": message}]})

    def test_certificate_diagnostic_needs_the_failure_exit_and_envelope(self):
        report = {"result": "unanswerable", "findings": [{"message": "invalid peer certificate: UnknownIssuer"}]}
        release.require_tls_refusal(1, report)
        release.require_tls_refusal(1, {**report, "findings": [{"message": "error performing TLS handshake"}]})
        for code, result in ((0, "unanswerable"), (2, "unanswerable"), (1, "ok"), (1, "findings")):
            with self.subTest(code=code, result=result), self.assertRaises(RuntimeError):
                release.require_tls_refusal(code, {**report, "result": result})

    def test_replaced_artifact_cannot_reach_any_product_command(self):
        class Replaced:
            windows = False
            bin = "/work/pbps"

            def exec(self, *args):
                return SimpleNamespace(stdout="b" * 64 + "  /work/pbps\n")

            def cli(self, *args, **kwargs):
                raise AssertionError("unverified artifact was executed")

        with self.assertRaisesRegex(RuntimeError, "differs from producer"):
            release.workload(Replaced(), "postgres", {}, "a" * 64)

    def test_process_failure_is_not_a_pass_and_redacts_fixture_password(self):
        with self.assertRaises(RuntimeError) as error:
            release.run(sys.executable, "-c", f"print({release.PASSWORD!r}); raise SystemExit(3)")
        self.assertIn("exited 3", str(error.exception))
        self.assertIn("[redacted]", str(error.exception))
        self.assertNotIn(release.PASSWORD, str(error.exception))

    def test_hung_consumer_has_a_finite_deadline(self):
        with self.assertRaisesRegex(RuntimeError, "deadline"):
            release.run(sys.executable, "-c", "import time; time.sleep(30)", timeout=0.1)


if __name__ == "__main__":
    unittest.main()
