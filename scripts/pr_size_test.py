#!/usr/bin/env python3
"""Tests for scripts/pr-size.py. Run: python3 scripts/pr_size_test.py"""

import importlib.util
import os
import subprocess
import tempfile
import unittest
from pathlib import Path

spec = importlib.util.spec_from_file_location(
    "pr_size", Path(__file__).with_name("pr-size.py")
)
ps = importlib.util.module_from_spec(spec)
spec.loader.exec_module(ps)

SOURCE = """\
pub fn shown() -> u8 {
    1
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shown_is_one() {
        assert_eq!(shown(), 1);
    }
}
"""


class Category(unittest.TestCase):
    def test_docs_and_markdown_count_toward_neither_budget(self):
        self.assertEqual(ps.category("docs/decisions/cli.md"), "docs")
        self.assertEqual(ps.category("AGENTS.md"), "docs")

    def test_fixtures_golden_data_and_script_tests_count_as_tests(self):
        for path in (
            "crates/pbps-cli/tests/flow.rs",
            "crates/pbps-pg/tests/fixtures/one.yaml",
            "crates/pbps-diff/src/order/tests.rs",
            "crates/pbps-diff/src/order_tests.rs",
            "crates/pbps-diff/src/scenes.golden",
            "scripts/pr_size_test.py",
        ):
            self.assertEqual(ps.category(path), "tests", path)

    def test_scripts_and_workflows_are_production(self):
        self.assertEqual(ps.category("scripts/pr-size.py"), "production")
        self.assertEqual(ps.category(".github/workflows/ci.yml"), "production")

    def test_a_name_merely_containing_test_is_not_a_test_file(self):
        self.assertEqual(ps.category("crates/pbps-cli/src/attest.rs"), "rust")
        self.assertEqual(ps.category("scripts/contest.py"), "production")


class TestLines(unittest.TestCase):
    def test_an_inline_cfg_test_module_counts_as_tests(self):
        self.assertEqual(ps.test_lines(SOURCE), set(range(5, 14)))

    def test_code_after_the_test_module_is_production(self):
        found = ps.test_lines(SOURCE + "\npub fn after() {}\n")
        self.assertNotIn(15, found)

    def test_extra_attributes_between_cfg_and_mod_stay_in_the_region(self):
        text = "#[cfg(test)]\n#[allow(dead_code)]\nmod tests {\n    fn a() {}\n}\n"
        self.assertEqual(ps.test_lines(text), {1, 2, 3, 4, 5})

    def test_cfg_test_on_an_item_that_is_not_a_module_opens_no_region(self):
        text = "#[cfg(test)]\nuse std::fmt;\n\npub fn real() {}\n"
        self.assertNotIn(4, ps.test_lines(text))

    def test_a_module_spelled_inside_a_raw_string_opens_no_region(self):
        text = (
            'const FIXTURE: &str = r#"\n'
            "#[cfg(test)]\n"
            "mod tests {\n"
            '"#;\n'
            "pub fn real() {}\n"
            "}\n"
        )
        self.assertEqual(ps.test_lines(text), set())

    def test_a_module_spelled_inside_a_block_comment_opens_no_region(self):
        text = "/*\n#[cfg(test)]\nmod tests {\n*/\npub fn real() {}\n}\n"
        self.assertEqual(ps.test_lines(text), set())

    def test_a_string_continuation_keeps_later_lines_aligned(self):
        text = SOURCE.replace(
            "        assert_eq!(shown(), 1);\n",
            '        let s = "one \\\n            two \\\n            three";\n'
            "        assert_eq!(shown(), 1);\n",
        )
        # Each continuation, if dropped, cuts one line off the end of the
        # file; two reach the module's closing brace.
        self.assertEqual(ps.test_lines(text), set(range(5, 17)))

    def test_a_quote_char_literal_does_not_open_a_string(self):
        text = "fn q() -> char {\n    '\"'\n}\n\n" + SOURCE
        self.assertEqual(ps.test_lines(text), set(range(9, 18)))


class Measure(unittest.TestCase):
    def test_a_branch_splits_into_production_tests_and_docs(self):
        with tempfile.TemporaryDirectory() as repo:
            env = dict(os.environ, GIT_AUTHOR_NAME="t", GIT_AUTHOR_EMAIL="t@t",
                       GIT_COMMITTER_NAME="t", GIT_COMMITTER_EMAIL="t@t")

            def run(*args):
                subprocess.run(args, cwd=repo, check=True, env=env,
                               capture_output=True)

            run("git", "init", "-q", "-b", "master")
            Path(repo, "lib.rs").write_text("pub fn a() {}\n")
            Path(repo, "q.sql").write_text("-- x\n")
            run("git", "add", ".")
            run("git", "commit", "-q", "-m", "base")
            run("git", "checkout", "-q", "-b", "topic")
            # One production line removed and one added, the test module, a
            # doc line, and a deleted SQL comment that reads like a header.
            Path(repo, "lib.rs").write_text("pub fn b() {}\n\n" + SOURCE)
            Path(repo, "notes.md").write_text("one\n")
            Path(repo, "q.sql").write_text("")
            run("git", "add", ".")
            run("git", "commit", "-q", "-m", "topic")
            cwd = os.getcwd()
            os.chdir(repo)
            try:
                totals = ps.measure("master")
            finally:
                os.chdir(cwd)
        # lib.rs: one deletion plus six added production lines (the new fn,
        # a blank, SOURCE's first four) and nine test lines; q.sql: one
        # deleted line, which must not be read as a file header.
        self.assertEqual(totals, {"production": 8, "tests": 9, "docs": 1})


if __name__ == "__main__":
    unittest.main()
