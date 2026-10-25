#!/usr/bin/env python3
"""Run: python3 scripts/findings_to_github_test.py (no network required)."""

import json
from pathlib import Path
import subprocess
import sys
import unittest

SCRIPT = Path(__file__).with_name("findings-to-github.py")


def render(report, markdown=False):
    return subprocess.run([sys.executable, str(SCRIPT)] + (["--pr-comment"] if markdown else []),
                          input=json.dumps(report), text=True, capture_output=True, check=False)


class Findings(unittest.TestCase):
    def test_both_modes_preserve_the_envelope_exit_code(self):
        for result, code in [("ok", 0), ("findings", 2), ("unanswerable", 1), ("unknown", 2)]:
            for markdown in [False, True]:
                with self.subTest(result=result, markdown=markdown):
                    output = render({"schema_version": 1, "result": result, "findings": []}, markdown)
                    self.assertEqual(output.returncode, code, output.stderr)
                    self.assertNotIn("::error", output.stdout if markdown else "")

    def test_markdown_lists_blockers_before_the_complete_fenced_batch(self):
        batch = 'pbps intent --drop app.t.a "" --drop-role reader ""'
        remedy = 'Use ```` inside a reason, then edit it.'
        report = {"schema_version": 1, "command": "plan", "result": "findings", "findings": [
            {"id": "identity.batch-required", "message": "Record together", "remedy": batch},
            {"id": "identity.drop", "message": "a disappeared", "remedy": remedy}]}
        output = render(report, True)
        self.assertEqual(output.returncode, 2)
        self.assertLess(output.stdout.index("a disappeared"), output.stdout.index(batch))
        self.assertIn(f"`````\n{remedy}\n`````", output.stdout)
        self.assertTrue(output.stdout.endswith(f"```\n{batch}\n```\n"))
        annotations = render(report)
        self.assertIn('::error title=pbps identity.drop::a disappeared%0A%0A', annotations.stdout)
        self.assertNotIn("##", annotations.stdout)

    def test_unknown_schema_versions_are_refused_in_both_modes(self):
        for markdown in [False, True]:
            output = render({"schema_version": 2, "result": "ok"}, markdown)
            self.assertEqual(output.returncode, 1)
            self.assertEqual(output.stdout, "")
            self.assertIn("reads findings schema 1", output.stderr)

    def test_missing_or_malformed_input_is_not_an_empty_success(self):
        for raw in ["", "{broken"]:
            for options in [[], ["--pr-comment"]]:
                output = subprocess.run([sys.executable, str(SCRIPT), *options], input=raw,
                                        text=True, capture_output=True, check=False)
                self.assertEqual(output.returncode, 1)
                self.assertTrue(output.stderr)


if __name__ == "__main__":
    unittest.main()
