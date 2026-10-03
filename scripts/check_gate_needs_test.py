#!/usr/bin/env python3
"""Tests for scripts/check_gate_needs.py. Run: python3 scripts/check_gate_needs_test.py"""

import importlib.util
import unittest
from pathlib import Path

spec = importlib.util.spec_from_file_location(
    "check_gate_needs", Path(__file__).with_name("check_gate_needs.py")
)
gn = importlib.util.module_from_spec(spec)
spec.loader.exec_module(gn)

WORKFLOW = """name: ci
on: push

jobs:
  # A comment between jobs.
  lint:
    runs-on: ubuntu-latest
    steps:
      - run: echo lint

  live:
    needs: lint
    services:
      mssql:
        image: x
    steps:
      - run: echo live

  gate:
    name: ci-gate
    needs: [lint, live]
    steps:
      - run: echo gate
"""


class GateNeeds(unittest.TestCase):
    def test_a_gate_that_waits_on_every_job_passes(self):
        self.assertEqual(gn.problems(WORKFLOW), [])

    def test_a_job_missing_from_the_gate_is_named(self):
        text = WORKFLOW.replace("needs: [lint, live]", "needs: [lint]")
        self.assertEqual(gn.problems(text), ["`gate.needs` is missing job `live`"])

    def test_a_new_job_added_without_the_gate_is_named(self):
        text = WORKFLOW.replace(
            "  gate:\n", "  extra:\n    steps:\n      - run: echo extra\n\n  gate:\n"
        )
        self.assertEqual(gn.problems(text), ["`gate.needs` is missing job `extra`"])

    def test_a_service_or_step_key_is_not_a_job(self):
        # `mssql:` under `services:` is indented deeper than a job key.
        self.assertNotIn("mssql", gn.jobs(WORKFLOW))

    def test_a_needs_entry_naming_no_job_is_named(self):
        text = WORKFLOW.replace("needs: [lint, live]", "needs: [lint, live, gone]")
        self.assertEqual(gn.problems(text), ["`gate.needs` names no such job `gone`"])

    def test_an_unreadable_needs_shape_fails_rather_than_passing(self):
        text = WORKFLOW.replace("needs: [lint, live]", "needs:\n      - lint\n      - live")
        self.assertEqual(gn.problems(text), ["`gate.needs` is not one `[a, b, ...]` list"])

    def test_a_gate_with_no_needs_fails_rather_than_passing(self):
        # Without `needs` the gate waits on nothing and its own success check
        # passes on an empty set, so the checker is the only thing left.
        text = WORKFLOW.replace("    needs: [lint, live]\n", "")
        self.assertEqual(gn.problems(text), ["`gate.needs` is not one `[a, b, ...]` list"])

    def test_a_missing_gate_fails_rather_than_passing(self):
        text = WORKFLOW.replace("  gate:\n", "  other:\n")
        self.assertEqual(gn.problems(text), ["no `gate` job"])

    def test_the_repository_workflow_passes(self):
        self.assertEqual(gn.problems(gn.WORKFLOW.read_text(encoding="utf-8")), [])


if __name__ == "__main__":
    unittest.main()
