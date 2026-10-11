#!/usr/bin/env python3
"""Tests for scripts/ci_release_scope.py. Run: python3 scripts/ci_release_scope_test.py"""

import importlib.util
import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

SCRIPT = Path(__file__).with_name("ci_release_scope.py")
spec = importlib.util.spec_from_file_location("ci_release_scope", SCRIPT)
rs = importlib.util.module_from_spec(spec)
spec.loader.exec_module(rs)

BASE = "a" * 40
HEAD = "b" * 40


def changes(*paths):
    return lambda base, head: list(paths)


class Decide(unittest.TestCase):
    def test_each_trigger_path_runs_the_full_qualification(self):
        for path in ("Cargo.toml", "Cargo.lock", "crates/pbps-cli/Cargo.toml", "rust-toolchain.toml",
                     ".cargo/config.toml", "crates/pbps-db/src/transport.rs", "scripts/qualify-release.py",
                     "scripts/release_qualification_test.py", "scripts/release-consumer-windows.Dockerfile",
                     "scripts/release-windows-fixture.ps1", "scripts/ci_release_scope.py",
                     ".github/workflows/ci.yml"):
            with self.subTest(path=path):
                run, reason = rs.decide("merge_group", BASE, HEAD, changes("docs/SPEC.md", path))
                self.assertTrue(run)
                self.assertIn(path, reason)

    def test_a_change_outside_the_trigger_set_skips(self):
        for paths in (("crates/pbps-core/src/lib.rs",), ("docs/SPEC.md", "AGENTS.md"),
                      ("crates/pbps-cli/src/main.rs", "crates/pbps-db-extra/x.rs"), ()):
            with self.subTest(paths=paths):
                run, _ = rs.decide("merge_group", BASE, HEAD, changes(*paths))
                self.assertFalse(run)

    def test_lookalike_paths_are_not_triggers(self):
        for path in ("docs/Cargo.toml.md", "crates/pbps-dbx/src/lib.rs", "scripts/qualify-release.py.bak",
                     "crates/pbps-cli/src/Cargo.toml", "x/.github/workflows/ci.yml"):
            with self.subTest(path=path):
                self.assertEqual(rs.triggered([path]), [])

    def test_schedule_and_dispatch_always_run_without_a_diff(self):
        def unreachable(base, head):
            raise AssertionError("no diff is read for an always-run event")
        for event in ("schedule", "workflow_dispatch"):
            with self.subTest(event=event):
                self.assertTrue(rs.decide(event, "", "", unreachable)[0])

    def test_any_other_event_runs_in_full(self):
        for event in ("pull_request", "push", "", "release"):
            with self.subTest(event=event):
                self.assertTrue(rs.decide(event, BASE, HEAD, changes())[0])

    def test_a_missing_or_malformed_base_or_head_runs_in_full(self):
        for base, head in (("", HEAD), (BASE, ""), ("abc", HEAD), (BASE, "B" * 40)):
            with self.subTest(base=base, head=head):
                self.assertTrue(rs.decide("merge_group", base, head, changes())[0])

    def test_an_unreadable_diff_runs_in_full(self):
        for error in (subprocess.CalledProcessError(128, ["git"]), OSError("git missing")):
            def failing(base, head, error=error):
                raise error
            with self.subTest(error=type(error).__name__):
                run, reason = rs.decide("merge_group", BASE, HEAD, failing)
                self.assertTrue(run)
                self.assertIn("could not be read", reason)


class Rename(unittest.TestCase):
    def git(self, *args):
        return subprocess.run(["git", "-c", "user.name=t", "-c", "user.email=t@example.invalid",
                               "-c", "commit.gpgsign=false", *args], cwd=self.root, check=True,
                              capture_output=True, text=True).stdout.strip()

    def test_a_rename_lists_both_names(self):
        with tempfile.TemporaryDirectory() as directory:
            self.root = directory
            self.git("init", "--quiet")
            (Path(directory) / "crates/pbps-db/src").mkdir(parents=True)
            (Path(directory) / "crates/pbps-db/src/tls.rs").write_text("fn f() {}\n" * 20)
            self.git("add", ".")
            self.git("commit", "--quiet", "-m", "base")
            base = self.git("rev-parse", "HEAD")
            (Path(directory) / "crates/pbps-core/src").mkdir(parents=True)
            self.git("mv", "crates/pbps-db/src/tls.rs", "crates/pbps-core/src/tls.rs")
            self.git("commit", "--quiet", "-m", "move out")
            head = self.git("rev-parse", "HEAD")
            paths = rs.diff_paths(base, head, cwd=directory)
            self.assertIn("crates/pbps-core/src/tls.rs", paths)
            self.assertEqual(rs.triggered(paths), ["crates/pbps-db/src/tls.rs"])
            self.assertTrue(rs.decide("merge_group", base, head,
                                      lambda b, h: rs.diff_paths(b, h, cwd=directory))[0])


    def test_a_non_ascii_or_quoted_name_is_read_as_its_path(self):
        with tempfile.TemporaryDirectory() as directory:
            self.root = directory
            self.git("init", "--quiet")
            self.git("commit", "--quiet", "--allow-empty", "-m", "base")
            base = self.git("rev-parse", "HEAD")
            fixtures = Path(directory) / "crates/pbps-db/tests/fixtures"
            fixtures.mkdir(parents=True)
            for name in ("caf\u00e9.pem", 'quote"d.pem', "tab\tname.pem"):
                (fixtures / name).write_text("x\n")
            self.git("add", ".")
            self.git("commit", "--quiet", "-m", "names Git would quote")
            head = self.git("rev-parse", "HEAD")
            paths = rs.diff_paths(base, head, cwd=directory)
            self.assertEqual(sorted(paths), sorted(f"crates/pbps-db/tests/fixtures/{n}"
                                                   for n in ("caf\u00e9.pem", 'quote"d.pem', "tab\tname.pem")))
            self.assertEqual(len(rs.triggered(paths)), 3)


class Main(unittest.TestCase):
    def test_outputs_and_summary_are_written(self):
        with tempfile.TemporaryDirectory() as directory:
            output, summary = Path(directory, "out"), Path(directory, "summary")
            env = dict(os.environ, EVENT="schedule", BASE_SHA="", HEAD_SHA="",
                       GITHUB_OUTPUT=str(output), GITHUB_STEP_SUMMARY=str(summary))
            result = subprocess.run([sys.executable, str(SCRIPT)], env=env, capture_output=True, text=True)
            self.assertEqual(result.returncode, 0)
            self.assertEqual(output.read_bytes(), b"run=true\n")
            self.assertIn("full qualification", summary.read_text())


if __name__ == "__main__":
    unittest.main()
