#!/usr/bin/env python3
"""Tests for scripts/gate_scope.py. Run: python3 scripts/gate_scope_test.py"""

import contextlib
import io
import json
import subprocess
import sys
import tempfile
import unittest
from collections import Counter
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import gate_scope as gs  # noqa: E402

GIT = ["git", "-c", "user.name=t", "-c", "user.email=t@t"]
LINES = [f"line {i}\n" for i in range(1, 9)]


class Repo(unittest.TestCase):
    """master moves on and the pull request is rebased on it, by hand-made trees."""

    def setUp(self):
        self.directory = tempfile.TemporaryDirectory(prefix="pbps-scope-")
        self.addCleanup(self.directory.cleanup)
        self.path = Path(self.directory.name)
        self.git("init", "-q", "-b", "main")
        self.write("src/lib.rs", "".join(LINES))
        self.write("docs/a.md", "doc\n")
        self.base = self.commit("base")

    def git(self, *args):
        return subprocess.run([*GIT, *args], cwd=self.path, text=True, capture_output=True, check=True).stdout.strip()

    def write(self, name, text):
        target = self.path / name
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text(text)

    def commit(self, message):
        self.git("add", "-A")
        self.git("commit", "-q", "--allow-empty", "-m", message)
        return self.git("rev-parse", "HEAD")

    def checkout(self, ref):
        self.git("checkout", "-q", "--detach", ref)

    def branch(self, start, files, message="change"):
        """A commit on `start` where `files` maps path -> text (None deletes)."""
        self.checkout(start)
        for name, text in files.items():
            if text is None:
                (self.path / name).unlink()
            else:
                self.write(name, text)
        return self.commit(message)

    def with_line(self, text, after, line):
        lines = text.splitlines(keepends=True)
        lines.insert(after, line)
        return "".join(lines)

    def verdict(self, old_head, new_base, new_head, old_base=None):
        return gs.verdict(self.path, [old_base or self.base, old_head, new_base, new_head])

    def pull_request(self):
        return self.branch(self.base, {"src/lib.rs": self.with_line("".join(LINES), 4, "pr line\n")})


class Unchanged(Repo):
    def test_an_adjacent_line_conflict_resolved_by_keeping_both_is_unchanged(self):
        old_head = self.pull_request()
        master = self.branch(self.base, {"src/lib.rs": self.with_line("".join(LINES), 4, "master line\n")}, "master")
        merged = self.with_line(self.with_line("".join(LINES), 4, "master line\n"), 5, "pr line\n")
        new_head = self.branch(master, {"src/lib.rs": merged})
        result = self.verdict(old_head, master, new_head)
        self.assertEqual(result["verdict"], "unchanged", result)
        self.assertEqual((result["files"], result["added"], result["removed"]), (1, 1, 0))
        self.assertRegex(result["digest"], "^[0-9a-f]{64}$")

    def test_the_digest_is_the_same_for_both_pairs_and_for_a_rerun(self):
        old_head = self.pull_request()
        master = self.branch(self.base, {"docs/a.md": "doc\nmore\n"}, "master")
        new_head = self.branch(master, {"src/lib.rs": self.with_line("".join(LINES), 4, "pr line\n")})
        first = self.verdict(old_head, master, new_head)
        self.assertEqual(first["verdict"], "unchanged")
        self.assertEqual(first["digest"], self.verdict(old_head, master, new_head)["digest"])
        self.assertEqual(first["digest"], gs.digest(gs.patch(self.path, self.base, old_head)))

    def test_the_same_lines_in_another_order_are_unchanged_by_design(self):
        old_head = self.branch(self.base, {"src/lib.rs": "".join(LINES) + "x\ny\n"})
        new_head = self.branch(self.base, {"src/lib.rs": "".join(LINES) + "y\nx\n"})
        self.assertEqual(self.verdict(old_head, self.base, new_head)["verdict"], "unchanged")


class Changed(Repo):
    def setUp(self):
        super().setUp()
        self.old_head = self.pull_request()
        self.master = self.branch(self.base, {"src/lib.rs": self.with_line("".join(LINES), 4, "master line\n")}, "master")
        self.merged = self.with_line(self.with_line("".join(LINES), 4, "master line\n"), 5, "pr line\n")

    def assertChanged(self, new_head, fragment, new_base=None):
        result = self.verdict(self.old_head, new_base or self.master, new_head)
        self.assertEqual(result["verdict"], "changed", result)
        self.assertIn(fragment, result["reason"])
        self.assertNotIn("digest", result)

    def test_a_changed_line_is_changed(self):
        self.assertChanged(self.branch(self.master, {"src/lib.rs": self.merged.replace("pr line", "pr line!")}),
                           "src/lib.rs")

    def test_an_added_line_is_changed(self):
        self.assertChanged(self.branch(self.master, {"src/lib.rs": self.merged + "extra\n"}), "src/lib.rs")

    def test_a_dropped_master_line_shows_as_a_removed_line(self):
        dropped = self.merged.replace("master line\n", "")
        self.assertChanged(self.branch(self.master, {"src/lib.rs": dropped}), "src/lib.rs")

    def test_a_dropped_pr_line_is_changed(self):
        self.assertChanged(self.branch(self.master, {"src/lib.rs": "".join(LINES)}), "src/lib.rs")

    def test_a_file_only_one_diff_touches_is_changed(self):
        new_head = self.branch(self.master, {"src/lib.rs": self.merged, "docs/a.md": "doc\nedited\n"})
        self.assertChanged(new_head, "docs/a.md")
        new_head = self.branch(self.master, {"src/lib.rs": self.merged})
        self.assertEqual(self.verdict(self.old_head, self.master, new_head)["verdict"], "unchanged")
        self.assertChanged(self.master, "src/lib.rs")  # the patch vanished

    def test_a_mode_change_is_changed(self):
        self.checkout(self.master)
        (self.path / "src/lib.rs").write_text(self.merged)
        (self.path / "src/lib.rs").chmod(0o755)
        new_head = self.commit("mode")
        self.assertChanged(new_head, "src/lib.rs")


class Blocked(Repo):
    def test_a_blocked_path_in_either_diff_is_changed_even_when_the_lines_match(self):
        for path in ("Cargo.lock", "Cargo.toml", "crates/x/Cargo.toml", "rust-toolchain.toml", ".github/workflows/ci.yml",
                     "scripts/live-tests.sh", "scripts/live-shards.py", "scripts/live_transport_test.py",
                     ".cargo/config.toml", "scripts/fixtures/pg-history695/x.sql", "scripts/gate_scope.py"):
            with self.subTest(path=path):
                old_head = self.branch(self.base, {path: "x\n"})
                master = self.branch(self.base, {"docs/a.md": "doc\nmaster\n"}, "master")
                new_head = self.branch(master, {path: "x\n"})
                result = self.verdict(old_head, master, new_head)
                self.assertEqual(result["verdict"], "changed")
                self.assertIn(path, result["reason"])

    def test_lookalike_paths_are_not_blocked(self):
        self.assertTrue(gs.decide({"docs/Cargo.toml.md": Counter({b"+x": 1})},
                                  {"docs/Cargo.toml.md": Counter({b"+x": 1})})[0])
        self.assertTrue(gs.decide({"crates/a/src/live-notes.rs": Counter({b"+x": 1})},
                                  {"crates/a/src/live-notes.rs": Counter({b"+x": 1})})[0])


class Refusals(Repo):
    def test_a_binary_patch_is_refused(self):
        self.write("blob.bin", "a\0b")
        base = self.commit("binary base")
        old_head = self.branch(base, {"blob.bin": "a\0c"})
        with self.assertRaisesRegex(gs.Refused, "binary patch"):
            gs.verdict(self.path, [base, old_head, base, old_head])

    def test_an_unreadable_ref_is_refused(self):
        head = self.pull_request()
        for refs in ([self.base, head, "0" * 40, head], [self.base, "no-such-branch", self.base, head],
                     ["", head, self.base, head]):
            with self.subTest(refs=refs):
                with self.assertRaises(gs.Refused):
                    gs.verdict(self.path, refs)

    def test_a_head_that_does_not_descend_from_its_base_is_refused(self):
        head = self.pull_request()
        other = self.branch(self.base, {"docs/a.md": "other\n"}, "other")
        with self.assertRaisesRegex(gs.Refused, "does not descend"):
            gs.verdict(self.path, [self.base, head, other, head])

    def test_a_directory_that_is_not_a_repository_is_refused(self):
        with tempfile.TemporaryDirectory() as other:
            with self.assertRaises(gs.Refused):
                gs.verdict(other, ["a" * 40] * 4)


class Command(Repo):
    def run_main(self, *refs):
        args = [f"--{name}={ref}" for name, ref in zip(("old-base", "old-head", "new-base", "new-head"), refs)]
        out = io.StringIO()
        with contextlib.redirect_stdout(out):
            code = gs.main([*args, f"--repo={self.path}"])
        return code, json.loads(out.getvalue())

    def test_exit_code_zero_only_for_unchanged_and_a_refusal_prints_json(self):
        head = self.pull_request()
        code, result = self.run_main(self.base, head, self.base, head)
        self.assertEqual((code, result["verdict"]), (0, "unchanged"))
        other = self.branch(self.base, {"src/lib.rs": "".join(LINES) + "z\n"})
        code, result = self.run_main(self.base, head, self.base, other)
        self.assertEqual((code, result["verdict"]), (1, "changed"))
        code, result = self.run_main(self.base, "missing", self.base, head)
        self.assertEqual((code, result["verdict"]), (1, "refused"))

    def test_a_missing_argument_is_a_usage_error(self):
        with contextlib.redirect_stderr(io.StringIO()), self.assertRaises(SystemExit) as raised:
            gs.main(["--old-base=a"])
        self.assertEqual(raised.exception.code, 2)


if __name__ == "__main__":
    unittest.main()
