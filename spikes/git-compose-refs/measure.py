#!/usr/bin/env python3
"""Measure ADR-0015's ref guards, not the unimplemented compose/recovery UI."""

import argparse
import os
from pathlib import Path
import select
import subprocess
import tempfile
import unittest


class Transaction:
    def __init__(self, fixture):
        self.fixture = fixture
        self.process = subprocess.Popen(
            [*fixture.git_command, "update-ref", "--stdin"], cwd=fixture.repo,
            env=fixture.env, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
            stderr=subprocess.PIPE, text=True,
        )
        fixture.addCleanup(self.close)
        assert self.exchange("start") == "start: ok"

    def exchange(self, command):
        self.process.stdin.write(command + "\n")
        self.process.stdin.flush()
        # Bound a failed experiment; do not hang forever on an unexpected Git.
        if not select.select([self.process.stdout], [], [], 10)[0]:
            raise AssertionError("Git did not acknowledge " + command)
        return self.process.stdout.readline().strip()

    def prepare(self):
        self.process.stdin.write(
            "option no-deref\nupdate refs/heads/a "
            + self.fixture.new + " " + self.fixture.old + "\n"
        )
        self.process.stdin.flush()
        return self.exchange("prepare") == "prepare: ok"

    def finish(self, command):
        self.fixture.assertEqual(self.exchange(command), command + ": ok")
        self.process.stdin.close()
        self.fixture.assertEqual(self.process.wait(timeout=10), 0)

    def close(self):
        if self.process.poll() is None:
            self.process.kill()
            self.process.wait(timeout=10)
        for pipe in [self.process.stdin, self.process.stdout, self.process.stderr]:
            pipe.close()


class RefProtocol(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(prefix="pbps-compose-ref-spike-")
        self.addCleanup(self.tmp.cleanup)
        self.repo = Path(self.tmp.name) / "repo"
        self.sibling = Path(self.tmp.name) / "sibling"
        self.repo.mkdir()
        hooks = Path(self.tmp.name) / "empty-hooks"
        hooks.mkdir()
        self.git_command = ["git", "-c", "core.hooksPath=" + str(hooks),
                            "-c", "core.fsmonitor=false"]
        self.env = {k: v for k, v in os.environ.items() if not k.startswith("GIT_")}
        self.env.update(
            GIT_CONFIG_NOSYSTEM="1", GIT_CONFIG_GLOBAL=os.devnull,
            GIT_TERMINAL_PROMPT="0", GIT_AUTHOR_NAME="Ref experiment",
            GIT_AUTHOR_EMAIL="ref-experiment@example.invalid",
            GIT_COMMITTER_NAME="Ref experiment",
            GIT_COMMITTER_EMAIL="ref-experiment@example.invalid",
        )
        self.git("init", "-q", "-b", "a")
        (self.repo / "f").write_text("original\n")
        self.git("add", "f")
        self.git("commit", "-qm", "original")
        self.old = self.git("rev-parse", "HEAD").stdout.strip()
        blob = self.git("hash-object", "-w", "--stdin", input="replacement\n").stdout.strip()
        tree = self.git("mktree", input="100644 blob " + blob + "\tf\n").stdout.strip()
        self.new = self.git("commit-tree", tree, "-p", self.old, input="composed\n").stdout.strip()
        self.git("branch", "b", self.old)
        self.git("worktree", "add", "-q", "--detach", str(self.sibling), self.old)
        self.index = self.repo / ".git/index"
        self.index_before = self.index.read_bytes()
        self.index_lock = self.repo / ".git/index.lock"
        self.index_lock.write_bytes(self.index_before)
        self.git("read-tree", self.new, extra_env={"GIT_INDEX_FILE": str(self.index_lock)})
        self.prepared_index = self.index_lock.read_bytes()
        self.head_lock = self.repo / ".git/HEAD.lock"
        self.head_lock.write_text("experiment-owner\n")
        self.branch_lock = self.repo / ".git/refs/heads/a.lock"

    def git(self, *args, input=None, cwd=None, ok=True, extra_env=None):
        result = subprocess.run(
            [*self.git_command, *args], cwd=cwd or self.repo,
            env={**self.env, **(extra_env or {})}, input=input,
            capture_output=True, text=True, timeout=10,
        )
        if ok:
            self.assertEqual(result.returncode, 0, result.stderr)
        return result

    def direct_branch(self):
        result = self.git("symbolic-ref", "-q", "refs/heads/a", ok=False)
        self.assertIn(result.returncode, [0, 1], result.stderr)
        # Quiet symbolic-ref's 1 means non-symbolic, not an arbitrary error.
        self.git("rev-parse", "--verify", "refs/heads/a^{commit}")
        return result.returncode == 1

    def postcondition(self):
        args = ["symbolic-ref", "-q"]
        if not OPTIONS.revert_head_guard:
            args.append("--no-recurse")
        head = self.git(*args, "HEAD", ok=False)
        return (
            head.returncode == 0 and head.stdout.strip() == "refs/heads/a"
            and self.direct_branch()
            and self.git("rev-parse", "refs/heads/a").stdout.strip() == self.new
        )

    def move_branch(self):
        self.head_lock.unlink()
        if OPTIONS.revert_type_guard:
            # Restore the whole old mechanism, not just a pre-check in a mock.
            self.git("update-ref", "--no-deref", "refs/heads/a", self.new, self.old)
            return True
        tx = Transaction(self)
        self.assertTrue(tx.prepare())
        direct = self.direct_branch()
        tx.finish("commit" if direct else "abort")
        return direct

    def take_persistent_locks(self):
        for path in [self.head_lock, self.branch_lock]:
            with path.open("x") as lock:
                lock.write("experiment-owner\n")

    def assert_index_uninstalled(self):
        self.assertEqual(self.index.read_bytes(), self.index_before)
        self.assertEqual(self.index_lock.read_bytes(), self.prepared_index)

    def test_direct_branch_commits_without_installing_the_prepared_index(self):
        self.assertTrue(self.move_branch())
        self.take_persistent_locks()
        self.assertTrue(self.postcondition())
        self.assertEqual(self.git("symbolic-ref", "--no-recurse", "HEAD").stdout.strip(), "refs/heads/a")
        self.assert_index_uninstalled()

    def test_same_tip_symbolic_rewrite_is_preserved_and_refused(self):
        self.git("symbolic-ref", "refs/heads/a", "refs/heads/b", cwd=self.sibling)
        before = (self.repo / ".git/refs/heads/a").read_bytes()
        committed = self.move_branch()
        self.assertEqual((self.repo / ".git/refs/heads/a").read_bytes(), before)
        self.assertFalse(committed, "same-tip symbolic rewrite was silently overwritten")
        self.assertEqual(self.git("rev-parse", "refs/heads/b").stdout.strip(), self.old)
        self.assertNotIn(self.new, self.git("rev-list", "--all").stdout.splitlines())
        self.assertFalse(self.branch_lock.exists())
        self.assertFalse(self.head_lock.exists())
        self.assert_index_uninstalled()

    def test_prepared_ref_type_is_visible_and_sibling_writes_are_locked(self):
        self.git("symbolic-ref", "refs/heads/a", "refs/heads/b", cwd=self.sibling)
        self.head_lock.unlink()
        tx = Transaction(self)
        self.assertTrue(tx.prepare())
        self.assertFalse(self.direct_branch())
        self.assertEqual(self.branch_lock.read_text().strip(), self.new)
        for args in [("symbolic-ref", "refs/heads/a", "refs/heads/b"),
                     ("update-ref", "--no-deref", "refs/heads/a", self.new, self.old)]:
            self.assertNotEqual(self.git(*args, cwd=self.sibling, ok=False).returncode, 0)
        tx.finish("abort")
        self.assertFalse(self.direct_branch())
        self.assert_index_uninstalled()

    def test_stale_tip_is_refused_for_direct_and_symbolic_branches(self):
        for symbolic in [False, True]:
            with self.subTest(symbolic=symbolic):
                self.git("update-ref", "--no-deref", "refs/heads/a", self.new, cwd=self.sibling)
                if symbolic:
                    self.git("update-ref", "refs/heads/b", self.new, cwd=self.sibling)
                    self.git("symbolic-ref", "refs/heads/a", "refs/heads/b", cwd=self.sibling)
                before = (self.repo / ".git/refs/heads/a").read_bytes()
                self.head_lock.unlink(missing_ok=True)
                tx = Transaction(self)
                self.assertFalse(tx.prepare())
                self.assertNotEqual(tx.process.wait(timeout=10), 0)
                self.assertIn("but expected", tx.process.stderr.read())
                self.assertEqual((self.repo / ".git/refs/heads/a").read_bytes(), before)
                self.assertFalse(self.branch_lock.exists())
                self.assert_index_uninstalled()

    def test_prepare_needs_head_lock_released_but_not_index_lock(self):
        tx = Transaction(self)
        self.assertFalse(tx.prepare())
        self.assertNotEqual(tx.process.wait(timeout=10), 0)
        self.assertIn("cannot lock ref 'HEAD'", tx.process.stderr.read())
        self.assertEqual(self.head_lock.read_text(), "experiment-owner\n")
        self.assertEqual(self.git("rev-parse", "refs/heads/a").stdout.strip(), self.old)
        self.assert_index_uninstalled()

    def test_head_chain_inserted_after_commit_fails_the_success_check(self):
        self.assertTrue(self.move_branch())
        self.git("symbolic-ref", "refs/heads/c", "refs/heads/a", cwd=self.sibling)
        # HEAD belongs to a worktree. The competing process explicitly writes
        # this checkout's HEAD, not the sibling's independent HEAD.
        self.git("symbolic-ref", "HEAD", "refs/heads/c")
        self.take_persistent_locks()
        self.assertEqual(self.git("symbolic-ref", "HEAD").stdout.strip(), "refs/heads/a")
        self.assertFalse(self.postcondition(), "recursive HEAD check accepted an unlocked hop")
        self.assert_index_uninstalled()
        self.git("symbolic-ref", "refs/heads/c", "refs/heads/b", cwd=self.sibling)
        self.assertEqual(self.git("rev-parse", "HEAD").stdout.strip(), self.old)
        self.assertEqual(self.git("rev-parse", "refs/heads/a").stdout.strip(), self.new)

    def test_head_chain_inserted_before_prepare_is_also_refused_after_commit(self):
        self.head_lock.unlink()
        self.git("symbolic-ref", "refs/heads/c", "refs/heads/a", cwd=self.sibling)
        self.git("symbolic-ref", "HEAD", "refs/heads/c")
        tx = Transaction(self)
        self.assertTrue(tx.prepare())
        self.assertTrue(self.direct_branch())
        tx.finish("commit")
        self.take_persistent_locks()
        self.assertFalse(self.postcondition(), "HEAD hop survived the transaction")
        self.assert_index_uninstalled()

    def test_post_write_branch_retarget_or_movement_is_still_refused(self):
        for symbolic in [False, True]:
            with self.subTest(symbolic=symbolic):
                self.head_lock.unlink(missing_ok=True)
                self.branch_lock.unlink(missing_ok=True)
                self.git("update-ref", "--no-deref", "refs/heads/a", self.old, cwd=self.sibling)
                self.head_lock.write_text("experiment-owner\n")
                self.assertTrue(self.move_branch())
                if symbolic:
                    self.git("update-ref", "refs/heads/b", self.new, cwd=self.sibling)
                    self.git("symbolic-ref", "refs/heads/a", "refs/heads/b", cwd=self.sibling)
                else:
                    self.git("update-ref", "refs/heads/a", self.old, cwd=self.sibling)
                self.take_persistent_locks()
                self.assertFalse(self.postcondition())
                self.assert_index_uninstalled()


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--revert-type-guard", action="store_true")
    parser.add_argument("--revert-head-guard", action="store_true")
    OPTIONS = parser.parse_args()
    print(subprocess.check_output(["git", "--version"], text=True).strip(), flush=True)
    print("Linux files-backend ref experiment; no UI compose/recovery implementation", flush=True)
    print("reverted type guard:", OPTIONS.revert_type_guard,
          "; reverted HEAD guard:", OPTIONS.revert_head_guard, flush=True)
    unittest.main(argv=[__file__], verbosity=2)
