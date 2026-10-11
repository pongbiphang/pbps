#!/usr/bin/env python3
"""Tests for scripts/ci_gate_verdict.py. Run: python3 scripts/ci_gate_verdict_test.py"""

import importlib.util
import json
import os
import subprocess
import sys
import unittest
from pathlib import Path

SCRIPT = Path(__file__).with_name("ci_gate_verdict.py")
spec = importlib.util.spec_from_file_location("ci_gate_verdict", SCRIPT)
gv = importlib.util.module_from_spec(spec)
spec.loader.exec_module(gv)

OMITTED = {"live", "resolver"}


def needs(**results):
    return {name.replace("_", "-"): {"result": result} for name, result in results.items()}


def full(result="success"):
    return needs(approve="success", lint="success", quick=result, live=result, resolver=result)


def reduced():
    return needs(approve="success", lint="success", quick="success", live="skipped", resolver="skipped")


class Verdict(unittest.TestCase):
    def test_a_pull_request_passes_with_exactly_the_omitted_jobs_skipped(self):
        self.assertEqual(gv.verdict("pull_request", reduced(), OMITTED), [])

    def test_every_other_event_passes_only_when_every_job_succeeded(self):
        for event in ("merge_group", "push", "schedule", "workflow_dispatch"):
            with self.subTest(event=event):
                self.assertEqual(gv.verdict(event, full(), OMITTED), [])

    def test_an_omitted_job_skipped_outside_a_pull_request_fails(self):
        for event in ("merge_group", "push", "schedule", "workflow_dispatch"):
            with self.subTest(event=event):
                errors = gv.verdict(event, reduced(), OMITTED)
                self.assertIn("live: skipped", errors)
                self.assertIn("resolver: skipped", errors)

    def test_an_omitted_job_that_ran_on_a_pull_request_fails_whatever_its_result(self):
        for result in ("success", "failure", "cancelled"):
            with self.subTest(result=result):
                results = reduced()
                results["live"] = {"result": result}
                errors = gv.verdict("pull_request", results, OMITTED)
                self.assertEqual(len(errors), 1)
                self.assertTrue(errors[0].startswith(f"live: {result}"))

    def test_an_unlisted_job_skipped_on_a_pull_request_fails(self):
        results = reduced()
        results["quick"] = {"result": "skipped"}
        self.assertEqual(gv.verdict("pull_request", results, OMITTED), ["quick: skipped"])

    def test_cancelled_and_failed_jobs_fail_on_every_event(self):
        for event in ("pull_request", "merge_group"):
            for result in ("failure", "cancelled"):
                with self.subTest(event=event, result=result):
                    results = reduced() if event == "pull_request" else full()
                    results["quick"] = {"result": result}
                    self.assertEqual(gv.verdict(event, results, OMITTED), [f"quick: {result}"])

    def test_an_unknown_event_fails_rather_than_choosing_a_verdict(self):
        for event in ("", "pull_request_target", "release"):
            with self.subTest(event=event):
                self.assertNotEqual(gv.verdict(event, full(), OMITTED), [])

    def test_an_empty_needs_fails(self):
        self.assertEqual(gv.verdict("merge_group", {}, OMITTED), ["the gate waits on no job"])

    def test_a_listed_job_the_gate_does_not_wait_on_fails(self):
        errors = gv.verdict("pull_request", reduced(), OMITTED | {"release-windows"})
        self.assertEqual(len(errors), 1)
        self.assertIn("release-windows", errors[0])

    def test_approval_and_lint_can_never_be_omitted(self):
        for name in ("approve", "lint"):
            with self.subTest(name=name):
                results = reduced()
                results[name] = {"result": "skipped"}
                errors = gv.verdict("pull_request", results, OMITTED | {name})
                self.assertIn(f"{name}: may never be omitted from a pull request", errors)

    def test_an_empty_list_makes_a_pull_request_need_every_job(self):
        self.assertEqual(gv.verdict("pull_request", full(), set()), [])
        self.assertIn("live: skipped", gv.verdict("pull_request", reduced(), set()))


class Main(unittest.TestCase):
    def run_script(self, event, needs_text, omitted="live resolver"):
        env = dict(os.environ, EVENT=event, NEEDS=needs_text, PR_OMITTED=omitted)
        return subprocess.run([sys.executable, str(SCRIPT)], env=env, capture_output=True, text=True)

    def test_the_exit_status_is_the_verdict(self):
        self.assertEqual(self.run_script("pull_request", json.dumps(reduced())).returncode, 0)
        failed = self.run_script("merge_group", json.dumps(reduced()))
        self.assertEqual(failed.returncode, 1)
        self.assertIn("::error::live: skipped", failed.stdout)

    def test_unreadable_needs_fails(self):
        for text in ("", "not json", "[]"):
            with self.subTest(text=text):
                self.assertEqual(self.run_script("merge_group", text).returncode, 1)


if __name__ == "__main__":
    unittest.main()
